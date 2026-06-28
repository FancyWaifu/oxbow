// Build xterm (roadmap A5) — a real upstream terminal X client — on oxbow. Reuses the whole
// X Toolkit stack from the xeyes/twm ports (libX11 + libxcb + libXau + libXext + libICE + libSM
// + libXt + libXmu), adds libXaw (Athena widgets — xterm's menus/scrollbar), then xterm itself.
// xterm runs core X fonts (the "fixed" font Xwayland serves from its libXfont2 builtins — no Xft,
// no fontconfig) and spawns /bin/sh over the kernel PTY via openpty (the personality's /dev/ptmx
// bridge, already proven by havoc's forkpty). Each library is its OWN cc::Build so its config.h
// resolves first. Sources out-of-repo at ~/musl-oxbow.
use std::path::Path;
use std::process::Command;

fn cfiles(dir: &str, exclude: &[&str]) -> Vec<String> {
    let mut v = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "c").unwrap_or(false) {
            let n = p.file_name().unwrap().to_str().unwrap().to_string();
            if !exclude.contains(&n.as_str()) { v.push(p.to_str().unwrap().to_string()); }
        }
    }
    v
}

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let mo = format!("{home}/musl-oxbow");
    let musl = format!("{mo}/musl-1.2.5");
    let out = std::env::var("OUT_DIR").unwrap();
    let pp = |s: &str| format!("{mo}/{s}");
    let (x11, xcb, xcbproto, xau, xp, xext, ice, sm, xt, xmu, xaw, xterm) = (
        pp("libX11"), pp("libxcb-1.16"), pp("xcb-proto-1.16.0"), pp("libXau"), pp("xorgproto/include"),
        pp("libXext-1.3.6"), pp("libICE-1.1.1"), pp("libSM-1.2.4"), pp("libXt-1.3.0"),
        pp("libXmu-1.2.1"), pp("libXaw-1.0.16"), pp("xterm-397"));

    assert!(Path::new(&xaw).exists(), "libXaw not found at {xaw}");
    assert!(Path::new(&xterm).exists(), "xterm not found at {xterm}");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed=build.rs");

    // --- generated headers (X11/Xt — same as the xeyes/twm builds) -----------------------
    if !Path::new(&format!("{x11}/src/ks_tables.h")).exists() {
        let mk = format!("{out}/makekeys");
        Command::new("clang").args(["-O2","-I",&xp,"-o",&mk,&format!("{x11}/src/util/makekeys.c")]).status().unwrap();
        let o = Command::new(&mk).arg(format!("{xp}/X11/keysymdef.h")).output().unwrap();
        std::fs::write(format!("{x11}/src/ks_tables.h"), o.stdout).unwrap();
    }
    if !Path::new(&format!("{x11}/include/X11/XlibConf.h")).exists() {
        let t = std::fs::read_to_string(format!("{x11}/include/X11/XlibConf.h.in")).unwrap()
            .replace("#undef XTHREADS","#define XTHREADS 1").replace("#undef XUSE_MTSAFE_API","#define XUSE_MTSAFE_API 1");
        std::fs::write(format!("{x11}/include/X11/XlibConf.h"), t).unwrap();
    }
    if !Path::new(&format!("{x11}/include/X11/Xpoll.h")).exists() {
        let t = std::fs::read_to_string(format!("{xp}/X11/Xpoll.h.in")).unwrap().replace("@USE_FDS_BITS@","fds_bits");
        std::fs::write(format!("{x11}/include/X11/Xpoll.h"), t).unwrap();
    }
    for pr in ["xproto","bigreq","xc_misc"] {
        if !Path::new(&format!("{xcb}/src/{pr}.c")).exists() {
            Command::new("python3").current_dir(format!("{xcb}/src"))
                .args(["./c_client.py","-c","libxcb 1.16","-l","X Version 11","-s","3","-p",&xcbproto,&format!("{xcbproto}/src/{pr}.xml")]).status().unwrap();
        }
    }
    if !Path::new(&format!("{xt}/src/StringDefs.c")).exists() {
        let mk = format!("{out}/makestrs");
        Command::new("clang").args(["-O2","-o",&mk,&format!("{xt}/util/makestrs.c")]).status().unwrap();
        let o = Command::new(&mk).args(["-i","."]).current_dir(&out)
            .stdin(std::process::Stdio::from(std::fs::File::open(format!("{xt}/util/string.list")).unwrap())).output().unwrap();
        std::fs::write(format!("{xt}/src/StringDefs.c"), o.stdout).unwrap();
        std::fs::copy(format!("{out}/StringDefs.h"), format!("{xt}/include/X11/StringDefs.h")).unwrap();
        std::fs::copy(format!("{out}/Shell.h"), format!("{xt}/include/X11/Shell.h")).unwrap();
    }
    // xterm parser tables: VTparse.h includes the generated VTparse.hin (#define CASE_* n) and
    // .cin ({n,"CASE_*"}) — produced from the .def files by the awk rules in xterm's Makefile.
    for base in ["VTparse", "Tekparse"] {
        let def = format!("{xterm}/{base}.def");
        if !Path::new(&format!("{xterm}/{base}.hin")).exists() {
            let o = Command::new("awk").arg("/^CASE_/{printf \"#define %s %d\\n\", $1, n++}").arg(&def).output().unwrap();
            std::fs::write(format!("{xterm}/{base}.hin"), o.stdout).unwrap();
        }
        if !Path::new(&format!("{xterm}/{base}.cin")).exists() {
            let o = Command::new("awk").arg("/^CASE_/{printf \"{ %d, \\\"%s\\\" },\\n\", n++, $1}").arg(&def).output().unwrap();
            std::fs::write(format!("{xterm}/{base}.cin"), o.stdout).unwrap();
        }
    }

    // xterm main.c patches for oxbow (we pass -D__oxbow__):
    //  1. USE_OPENPTY platform gate → get_pty() uses openpty() (the /dev/ptmx-bridge path).
    //  2. include <pty.h> for openpty()'s declaration.
    //  3. the BSD child-pgrp path uses setpgrp(0,0)/(0,pgrp) (4.x BSD 2-arg); musl has the
    //     POSIX setpgrp(void). We keep the BSD path (it opens the pts slave directly — no
    //     /dev/tty, which the SysV path needs) and call the 0-arg form. setsid()+TIOCSCTTY
    //     (already in that block) establish the controlling terminal, like login_tty.
    {
        let mc = format!("{xterm}/main.c");
        let s = std::fs::read_to_string(&mc).unwrap();
        if !s.contains("defined(__oxbow__)") {
            let mut t = s.replace("|| defined(__APPLE__)\n#define USE_OPENPTY 1",
                                  "|| defined(__APPLE__) || defined(__oxbow__)\n#define USE_OPENPTY 1");
            t = t.replace("#if defined(__FreeBSD__) || defined(__DragonFly__)\n#include <libutil.h>		/* openpty() */\n#endif",
                          "#if defined(__FreeBSD__) || defined(__DragonFly__)\n#include <libutil.h>		/* openpty() */\n#endif\n\n#if defined(__oxbow__)\n#include <pty.h>		/* openpty() */\n#endif");
            t = t.replace("\t    setpgrp(0, 0);", "\t    setpgrp();");
            t = t.replace("\t    setpgrp(0, pgrp);", "\t    setpgrp();");
            // Tolerate ENOSYS in the "no controlling terminal" check: oxbow has no /dev/tty and
            // returns ENXIO for it, but a following signal()/alarm() (unimplemented → ENOSYS)
            // can clobber errno before xterm inspects it. Treat ENOSYS like ENXIO (use defaults)
            // instead of the fatal ERROR_OPDEVTTY.
            t = t.replace("errno == EINVAL || errno == ENOTTY || errno == EACCES) {",
                          "errno == EINVAL || errno == ENOTTY || errno == EACCES || errno == ENOSYS) {");
            assert!(t != s, "xterm main.c patch anchors not found — layout changed");
            std::fs::write(&mc, t).unwrap();
        }
    }

    // --- config.h per library ------------------------------------------------------------
    let w = |path: String, s: &str| std::fs::write(path, s).unwrap();
    w(format!("{x11}/src/config.h"),
      "#define XTHREADS 1\n#define XUSE_MTSAFE_API 1\n#define HAVE_UNISTD_H 1\n#define HAVE_SYS_SOCKET_H 1\n\
       #define HAVE_SYS_SELECT_H 1\n#define HAVE_SYS_IOCTL_H 1\n#define HAVE_INTTYPES_H 1\n#define HAVE___BUILTIN_POPCOUNTL 1\n\
       #define HAVE_GETADDRINFO 1\n#define ERRORDB \"/usr/share/X11/XErrorDB\"\n#define XLOCALEDIR \"/usr/share/X11/locale\"\n\
       #define XLOCALELIBDIR \"/usr/lib/X11/locale\"\n#define XKBDIR \"/usr/share/X11/xkb\"\n#define XCMSDIR \"/usr/share/X11\"\n");
    w(format!("{xcb}/src/config.h"), "#define HAVE_GETADDRINFO 1\n#define HAVE_SENDMSG 1\n#define XCB_QUEUE_BUFFER_SIZE 16384\n");
    w(format!("{xext}/src/config.h"), "#define HAVE_SYS_TYPES_H 1\n#define MALLOC_0_RETURNS_NULL 1\n");
    w(format!("{ice}/src/config.h"), "#define HAVE_ASPRINTF 1\n#define HAVE_UNISTD_H 1\n#define HAVE_SYS_TIME_H 1\n#define HAVE_GETADDRINFO 1\n#define ICEAUTHDIR \"/tmp\"\n");
    w(format!("{sm}/src/config.h"), "#define HAVE_ASPRINTF 1\n#define HAVE_GETADDRINFO 1\n#define HAVE_UNISTD_H 1\n");
    w(format!("{xt}/src/config.h"),
      "#define XTHREADS 1\n#define XUSE_MTSAFE_API 1\n#define HAVE_UNISTD_H 1\n#define HAVE_ASPRINTF 1\n#define HAVE_MMAP 1\n\
       #define XFILESEARCHPATHDEFAULT \"/usr/share/X11/%T/%N%S\"\n#define ERRORDB \"/usr/share/X11/XtErrorDB\"\n");
    w(format!("{xmu}/src/config.h"), "#define HAVE_UNISTD_H 1\n#define HAVE_ASPRINTF 1\n");
    // libXaw — only 6 config symbols are actually referenced in src/.
    w(format!("{xaw}/src/config.h"),
      "#define HAVE_UNISTD_H 1\n#define HAVE_WCHAR_H 1\n#define HAVE_WCTYPE_H 1\n\
       #define HAVE_ISWALNUM 1\n#define HAVE_GETPAGESIZE 1\n");
    // xterm <xtermcfg.h>: openpty pty path, core fonts (no Xft/XRENDERFONT), wide/i18n/tek/graphics
    // OFF to keep deps minimal. Unlisted OPT_* take xterm's header defaults (mostly enabled).
    w(format!("{xterm}/xtermcfg.h"),
      "#ifndef included_xtermcfg_h\n#define included_xtermcfg_h 1\n\
       #define HAVE_UNISTD_H 1\n#define HAVE_STDLIB_H 1\n#define HAVE_STDINT_H 1\n#define HAVE_TERMIOS_H 1\n\
       #define HAVE_SYS_TIME_H 1\n#define HAVE_SYS_WAIT_H 1\n#define HAVE_SYS_PARAM_H 1\n#define HAVE_WCHAR_H 1\n\
       #define HAVE_PTY_H 1\n#define HAVE_TCGETATTR 1\n#define HAVE_WAITPID 1\n#define HAVE_SETSID 1\n\
       #define HAVE_SETPGID 1\n#define HAVE_PUTENV 1\n#define HAVE_UNSETENV 1\n#define HAVE_STRFTIME 1\n\
       #define HAVE_GETTIMEOFDAY 1\n#define HAVE_GETHOSTNAME 1\n#define HAVE_GETLOGIN 1\n#define HAVE_MKSTEMP 1\n\
       #define HAVE_SCHED_YIELD 1\n#define HAVE_SYS_TTYDEFAULTS_H 1\n#define USE_POSIX_WAIT 1\n\
       #define HAVE_LIB_XAW 1\n#define USE_POSIX_TERMIOS 1\n#define DFT_TERMTYPE \"xterm\"\n#define DEFDELETE_DEL 1\n\
       #define OPT_I18N_SUPPORT 0\n#define OPT_INPUT_METHOD 0\n#define OPT_WIDE_CHARS 0\n\
       #define OPT_TEK4014 0\n#define OPT_TOOLBAR 0\n#define OPT_REGIS_GRAPHICS 0\n#define OPT_SIXEL_GRAPHICS 0\n\
       #define OPT_GRAPHICS 0\n#define OPT_SCREEN_DUMPS 0\n#define OPT_DEC_LOCATOR 0\n#define OPT_DABBREV 0\n\
       #define OPT_SELECT_REGEX 0\n#define OPT_SESSION_MGT 0\n#define NO_ACTIVE_ICON 1\n\
       #ifndef HAVE_X11_XPOLL_H\n#define NO_XPOLL_H\n#endif\n#endif\n");

    // --- staging (xcb headers + xtrans symlink) -----------------------------------------
    let xcbinc = format!("{out}/xcbinc/xcb");
    std::fs::create_dir_all(&xcbinc).unwrap();
    for h in ["xcb","xcbext","xproto","bigreq","xc_misc"] { std::fs::copy(format!("{xcb}/src/{h}.h"), format!("{xcbinc}/{h}.h")).unwrap(); }
    std::fs::create_dir_all(format!("{out}/xtstage/X11")).unwrap();
    let _ = std::os::unix::fs::symlink(pp("xtrans"), format!("{out}/xtstage/X11/Xtrans"));
    // Minimal <X11/xpm.h> stub: libXaw's Pixmap.c includes it for XPM-FILE backgrounds only.
    // xterm never loads XPM files, so the loader always fails — but the header must satisfy the
    // field/symbol references so XawPixmapFromXPixmap (which does NOT use xpm) still compiles.
    std::fs::create_dir_all(format!("{out}/xpmstub/X11")).unwrap();
    w(format!("{out}/xpmstub/X11/xpm.h"),
      "#ifndef OX_XPM_STUB_H\n#define OX_XPM_STUB_H\n#include <X11/Xlib.h>\n\
       #define XpmSize 0x10\n#define XpmColormap 0x40\n#define XpmCloseness 0x400\n#define XpmSuccess 0\n\
       typedef struct { unsigned long valuemask; Colormap colormap; unsigned int closeness;\n\
       unsigned int width; unsigned int height; } XpmAttributes;\n\
       static inline int XpmReadFileToPixmap(Display *d, Drawable da, char *f, Pixmap *p,\n\
       Pixmap *m, XpmAttributes *a){ (void)d;(void)da;(void)f;(void)p;(void)m;(void)a; return -1; }\n#endif\n");
    // Minimal <curses.h>: xterm's xtermcap.h unconditionally includes it for termcap. xterm is the
    // TERMINAL — it doesn't need a termcap DB; the queries (only for tcap-derived function keys)
    // fail gracefully. Declarations here; failing impls in termcap_glue.c.
    std::fs::create_dir_all(format!("{out}/cursestub")).unwrap();
    w(format!("{out}/cursestub/curses.h"),
      "#ifndef OX_CURSES_STUB\n#define OX_CURSES_STUB\n#define OK 0\n#define ERR (-1)\n\
       extern int tgetent(char *bp, const char *name);\nextern char *tgetstr(const char *id, char **area);\n\
       extern int tgetnum(const char *id);\nextern int tgetflag(const char *id);\n\
       extern char *tgoto(const char *cap, int col, int row);\n\
       extern int tputs(const char *str, int affcnt, int (*putc)(int));\n#endif\n");
    w(format!("{out}/termcap_glue.c"),
      "int tgetent(char *bp, const char *name){ (void)name; if(bp) bp[0]=0; return 0; }\n\
       char *tgetstr(const char *id, char **area){ (void)id; (void)area; return 0; }\n\
       int tgetnum(const char *id){ (void)id; return -1; }\n\
       int tgetflag(const char *id){ (void)id; return 0; }\n\
       char *tgoto(const char *cap, int col, int row){ (void)cap;(void)col;(void)row; return (char*)\"\"; }\n\
       int tputs(const char *str, int affcnt, int (*pc)(int)){ (void)affcnt; while(str&&*str) pc(*str++); return 0; }\n");

    // --- link recipe + per-build base ---------------------------------------------------
    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");
    let sysroot = String::from_utf8(Command::new("rustc").args(["--print","sysroot"]).output().unwrap().stdout).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout).unwrap();
    let res_inc = format!("{}/include", res.trim());

    let xcbinc_dir = format!("{out}/xcbinc");
    let xtstage = format!("{out}/xtstage");
    let base = || {
        let mut b = cc::Build::new();
        b.compiler("clang").archiver(&llvm_ar)
            .flag("-nostdinc").flag("-isystem").flag(&res_inc)
            .define("HAVE_CONFIG_H", None).define("_GNU_SOURCE", None)
            .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin").flag("-Wno-everything").opt_level(2);
        b
    };
    let shared = |b: &mut cc::Build| {
        b.include(format!("{x11}/include")).include(format!("{x11}/include/X11"))
         .include(&xcbinc_dir).include(format!("{xau}/include")).include(&xtstage).include(&xp)
         .include(format!("{musl}/include")).include(format!("{musl}/obj/include"))
         .include(format!("{musl}/arch/x86_64")).include(format!("{musl}/arch/generic"));
    };

    // libxcb + libXau
    let mut bxcb = base(); bxcb.include(format!("{xcb}/src")); shared(&mut bxcb);
    for f in ["xcb_conn","xcb_out","xcb_in","xcb_ext","xcb_xid","xcb_list","xcb_util","xcb_auth","xproto","bigreq","xc_misc"] { bxcb.file(format!("{xcb}/src/{f}.c")); }
    for f in ["AuDispose","AuFileName","AuGetAddr","AuGetBest","AuLock","AuRead","AuUnlock","AuWrite"] { bxcb.file(format!("{xau}/{f}.c")); }
    bxcb.compile("ox_xcb");

    // libX11
    let mut bx = base();
    bx.include(format!("{x11}/src")).include(format!("{x11}/src/xcms")).include(format!("{x11}/src/xlibi18n"))
      .include(format!("{x11}/src/xkb")).include(format!("{x11}/modules/im/ximcp")).include(format!("{x11}/modules/om/generic"));
    shared(&mut bx);
    for f in cfiles(&format!("{x11}/src"), &[]) { bx.file(f); }
    for f in cfiles(&format!("{x11}/src/xcms"), &[]) { bx.file(f); }
    for f in cfiles(&format!("{x11}/src/xkb"), &[]) { bx.file(f); }
    for f in cfiles(&format!("{x11}/src/xlibi18n"), &["XlcDL.c","xim_trans.c"]) { bx.file(f); }
    for f in ["gen/lcGenConv","def/lcDefConv","Utf8/lcUTF8Load"] { bx.file(format!("{x11}/modules/lc/{f}.c")); }
    for f in cfiles(&format!("{x11}/modules/om/generic"), &[]) { bx.file(f); }
    for f in cfiles(&format!("{x11}/modules/im/ximcp"), &["imTrans.c"]) { bx.file(f); }
    bx.compile("ox_x11");

    // libXext
    let mut be = base(); be.include(format!("{xext}/src")).include(format!("{xext}/include")); shared(&mut be);
    for f in cfiles(&format!("{xext}/src"), &[]) { be.file(f); }
    be.compile("ox_xext");

    // libICE
    let mut bi = base(); bi.define("ICE_t", None).define("TRANS_CLIENT", None).define("TRANS_SERVER", None)
        .include(format!("{ice}/src")).include(format!("{ice}/include")); shared(&mut bi);
    for f in cfiles(&format!("{ice}/src"), &[]) { bi.file(f); }
    bi.compile("ox_ice");

    // libSM
    let mut bs = base(); bs.define("SM_t", None).define("TRANS_CLIENT", None)
        .include(format!("{sm}/src")).include(format!("{sm}/include")).include(format!("{ice}/include")); shared(&mut bs);
    for f in cfiles(&format!("{sm}/src"), &[]) { bs.file(f); }
    bs.compile("ox_sm");

    // libXt
    let mut bt = base();
    bt.include(format!("{xt}/src")).include(format!("{xt}/include")).include(format!("{xt}/include/X11"))
      .include(format!("{ice}/include")).include(format!("{sm}/include"));
    shared(&mut bt);
    for f in cfiles(&format!("{xt}/src"), &[]) { bt.file(f); }
    bt.compile("ox_xt");

    // libXmu
    let mut bm = base();
    bm.include(format!("{xmu}/src")).include(format!("{xmu}/include")).include(format!("{xmu}/include/X11/Xmu"))
      .include(format!("{xt}/include")).include(format!("{xt}/include/X11")).include(format!("{xext}/include"))
      .include(format!("{sm}/include")).include(format!("{ice}/include"));
    shared(&mut bm);
    for f in cfiles(&format!("{xmu}/src"), &["Xct.c"]) { bm.file(f); }
    bm.compile("ox_xmu");

    // libXaw (Athena widgets — xterm's menus + scrollbar)
    let mut bw = base();
    bw.include(format!("{out}/xpmstub")).include(format!("{xaw}/src")).include(format!("{xaw}/include"))
      .include(format!("{xt}/include")).include(format!("{xt}/include/X11"))
      .include(format!("{xmu}/include")).include(format!("{xext}/include"))
      .include(format!("{sm}/include")).include(format!("{ice}/include"));
    shared(&mut bw);
    for f in cfiles(&format!("{xaw}/src"), &[]) { bw.file(f); }
    bw.compile("ox_xaw");

    // xterm itself + the musl personality
    let xterm_exclude = [
        "resize.c",        // a separate program, not part of xterm
        "Tekproc.c", "TekPrsTbl.c", // tek4014 disabled
        "graphics.c", "graphics_regis.c", "graphics_sixel.c", // graphics disabled
        "svg.c", "html.c", // screen dumps disabled
        "testxmc.c", "trace.c", // test/trace helpers
        "precompose.c", "xutf8.c", "keysym2ucs.c", // wide-char/i18n disabled
    ];
    let mut ba = base();
    ba.define("OXBOW_ARGV0", "\"xterm\"").define("__oxbow__", None)
      .include(&xterm).include(&pers).include(format!("{out}/cursestub"))
      .include(format!("{xaw}/include"))
      .include(format!("{xt}/include")).include(format!("{xt}/include/X11"))
      .include(format!("{xext}/include")).include(format!("{xmu}/include"))
      .include(format!("{sm}/include")).include(format!("{ice}/include"));
    shared(&mut ba);
    for f in cfiles(&xterm, &xterm_exclude) { ba.file(f); }
    ba.file(format!("{out}/termcap_glue.c"));
    ba.file(format!("{pers}/crt_glue.c")).file(format!("{pers}/oxbow_syscall.c"));
    ba.compile("ox_app");
}
