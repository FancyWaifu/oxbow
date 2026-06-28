// Build the first UNMODIFIED UPSTREAM X app — xeyes — on oxbow (roadmap A3). Compiles the
// whole X Toolkit stack against musl: libX11 + libxcb + libXau (A1/A2) + libXext + libICE +
// libSM + libXt + libXmu, then xeyes (--without-xrender). Each library is its OWN cc::Build
// (separate static lib) so its config.h resolves first and per-lib defines (ICE_t vs SM_t,
// which namespace the shared xtrans transport) don't collide. Sources out-of-repo at
// ~/musl-oxbow; generated headers produced at build time if missing.
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
    let (x11, xcb, xcbproto, xau, xp, xext, ice, sm, xt, xmu, twm) = (
        pp("libX11"), pp("libxcb-1.16"), pp("xcb-proto-1.16.0"), pp("libXau"), pp("xorgproto/include"),
        pp("libXext-1.3.6"), pp("libICE-1.1.1"), pp("libSM-1.2.4"), pp("libXt-1.3.0"),
        pp("libXmu-1.2.1"), pp("twm-1.0.12"));

    assert!(Path::new(&xt).exists(), "libXt not found at {xt}");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed=build.rs");

    // --- generated headers --------------------------------------------------------------
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

    // --- config.h per library -----------------------------------------------------------
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
    // twm: core X fonts, no xrandr (HAVE_XRANDR left undefined)
    w(format!("{twm}/src/config.h"),
      "#define HAVE_UNISTD_H 1\n#define HAVE_SYS_TIME_H 1\n#define HAVE_MKSTEMP 1\n#define HAVE__XEATDATAWORDS 1\n\
       #define APP_NAME \"twm\"\n#define XVENDORNAME \"oxbow\"\n#define PACKAGE_VERSION \"1.0.12\"\n");

    // --- staging ------------------------------------------------------------------------
    let xcbinc = format!("{out}/xcbinc/xcb");
    std::fs::create_dir_all(&xcbinc).unwrap();
    for h in ["xcb","xcbext","xproto","bigreq","xc_misc"] { std::fs::copy(format!("{xcb}/src/{h}.h"), format!("{xcbinc}/{h}.h")).unwrap(); }
    std::fs::create_dir_all(format!("{out}/xtstage/X11")).unwrap();
    let _ = std::os::unix::fs::symlink(pp("xtrans"), format!("{out}/xtstage/X11/Xtrans"));

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

    // a fresh base Build (common flags + shared trailing includes: xorgproto, xcb, musl)
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
    // shared trailing includes appended AFTER each lib's own dirs
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

    // libX11 (+ subsystems + locale/om/im modules)
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

    // libICE (xtrans transport, namespaced by ICE_t)
    let mut bi = base(); bi.define("ICE_t", None).define("TRANS_CLIENT", None).define("TRANS_SERVER", None)
        .include(format!("{ice}/src")).include(format!("{ice}/include")); shared(&mut bi);
    for f in cfiles(&format!("{ice}/src"), &[]) { bi.file(f); }
    bi.compile("ox_ice");

    // libSM (xtrans transport, namespaced by SM_t)
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

    // twm (the upstream window manager) + the musl personality
    let mut ba = base();
    ba.define("OXBOW_ARGV0", "\"twm\"")
      // branding macros normally passed via -D by the xorg build (not in config.h)
      .define("APP_NAME", "\"twm\"").define("APP_VERSION", "\"1.0.12\"")
      .define("XVENDORNAME", "\"oxbow\"").define("XORG_RELEASE", "\"oxbow X\"")
      .include(format!("{twm}/src")).include(&pers)
      .include(format!("{xt}/include")).include(format!("{xt}/include/X11"))
      .include(format!("{xext}/include")).include(format!("{xmu}/include"))
      .include(format!("{sm}/include")).include(format!("{ice}/include"));
    shared(&mut ba);
    for f in cfiles(&format!("{twm}/src"), &[]) { ba.file(f); }
    ba.file(format!("{pers}/crt_glue.c")).file(format!("{pers}/oxbow_syscall.c"));
    ba.compile("ox_app");
}
