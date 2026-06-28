// Build a minimal X client on REAL libX11/Xlib (roadmap A2), on top of the libxcb
// transport from A1. Compiles libX11 (core + xcms + xkb + most i18n) + libxcb + libXau +
// the Xlib client + the musl personality, linked against musl libc.a. Sources out-of-repo
// at ~/musl-oxbow. Generated headers (ks_tables.h, XlibConf.h, Xpoll.h) are produced at
// build time if missing.
use std::path::Path;
use std::process::Command;

fn cfiles(dir: &str, exclude: &[&str]) -> Vec<String> {
    let mut v = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "c").unwrap_or(false) {
            let name = p.file_name().unwrap().to_str().unwrap().to_string();
            if !exclude.contains(&name.as_str()) {
                v.push(p.to_str().unwrap().to_string());
            }
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
    let x11 = format!("{mo}/libX11");
    let xcb = format!("{mo}/libxcb-1.16");
    let xcbproto = format!("{mo}/xcb-proto-1.16.0");
    let xau = format!("{mo}/libXau");
    let xp = format!("{mo}/xorgproto/include");
    let out = std::env::var("OUT_DIR").unwrap();

    assert!(Path::new(&x11).exists(), "libX11 not found at {x11}");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed=xlibdemo.c");
    println!("cargo:rerun-if-changed=build.rs");

    // --- generated/config headers -------------------------------------------------------
    // ks_tables.h via the host makekeys tool
    if !Path::new(&format!("{x11}/src/ks_tables.h")).exists() {
        let mk = format!("{out}/makekeys");
        assert!(Command::new("clang").args(["-O2", "-I", &xp, "-o", &mk,
            &format!("{x11}/src/util/makekeys.c")]).status().unwrap().success(), "build makekeys");
        let o = Command::new(&mk).arg(format!("{xp}/X11/keysymdef.h")).output().unwrap();
        std::fs::write(format!("{x11}/src/ks_tables.h"), o.stdout).unwrap();
    }
    // XlibConf.h (threads on)
    if !Path::new(&format!("{x11}/include/X11/XlibConf.h")).exists() {
        let t = std::fs::read_to_string(format!("{x11}/include/X11/XlibConf.h.in")).unwrap()
            .replace("#undef XTHREADS", "#define XTHREADS 1")
            .replace("#undef XUSE_MTSAFE_API", "#define XUSE_MTSAFE_API 1");
        std::fs::write(format!("{x11}/include/X11/XlibConf.h"), t).unwrap();
    }
    // Xpoll.h (musl fd_set member = fds_bits)
    if !Path::new(&format!("{x11}/include/X11/Xpoll.h")).exists() {
        let t = std::fs::read_to_string(format!("{xp}/X11/Xpoll.h.in")).unwrap()
            .replace("@USE_FDS_BITS@", "fds_bits");
        std::fs::write(format!("{x11}/include/X11/Xpoll.h"), t).unwrap();
    }
    // config.h for libX11
    std::fs::write(format!("{x11}/src/config.h"),
        "#define XTHREADS 1\n#define XUSE_MTSAFE_API 1\n#define HAVE_UNISTD_H 1\n\
         #define HAVE_SYS_SOCKET_H 1\n#define HAVE_SYS_SELECT_H 1\n#define HAVE_SYS_IOCTL_H 1\n\
         #define HAVE_INTTYPES_H 1\n#define HAVE___BUILTIN_POPCOUNTL 1\n#define HAVE_GETADDRINFO 1\n\
         #define ERRORDB \"/usr/share/X11/XErrorDB\"\n#define XLOCALEDIR \"/usr/share/X11/locale\"\n\
         #define XLOCALELIBDIR \"/usr/lib/X11/locale\"\n#define XKBDIR \"/usr/share/X11/xkb\"\n\
         #define XCMSDIR \"/usr/share/X11\"\n").unwrap();
    // config.h for libxcb (it shares the cc group's -I src order; give it its own)
    std::fs::write(format!("{xcb}/src/config.h"),
        "#define HAVE_GETADDRINFO 1\n#define HAVE_SENDMSG 1\n#define XCB_QUEUE_BUFFER_SIZE 16384\n").unwrap();
    // ensure libxcb protocol files generated
    for p in ["xproto", "bigreq", "xc_misc"] {
        if !Path::new(&format!("{xcb}/src/{p}.c")).exists() {
            assert!(Command::new("python3").current_dir(format!("{xcb}/src"))
                .args(["./c_client.py", "-c", "libxcb 1.16", "-l", "X Version 11", "-s", "3",
                       "-p", &xcbproto, &format!("{xcbproto}/src/{p}.xml")]).status().unwrap().success());
        }
    }

    // stage xcb headers under an `xcb/` dir
    let xcbinc = format!("{out}/xcbinc/xcb");
    std::fs::create_dir_all(&xcbinc).unwrap();
    for h in ["xcb", "xcbext", "xproto", "bigreq", "xc_misc"] {
        std::fs::copy(format!("{xcb}/src/{h}.h"), format!("{xcbinc}/{h}.h")).unwrap();
    }

    // --- link recipe --------------------------------------------------------------------
    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");

    let sysroot = String::from_utf8(Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout).unwrap();
    let res_inc = format!("{}/include", res.trim());

    let mut b = cc::Build::new();
    b.compiler("clang").archiver(&llvm_ar)
        .flag("-nostdinc").flag("-isystem").flag(&res_inc)
        .include(format!("{x11}/src")).include(format!("{x11}/src/xcms"))
        .include(format!("{x11}/src/xlibi18n")).include(format!("{x11}/src/xkb"))
        .include(format!("{x11}/modules/im/ximcp")).include(format!("{x11}/modules/om/generic"))
        .include(format!("{x11}/include")).include(format!("{x11}/include/X11"))
        .include(format!("{xcb}/src")).include(format!("{out}/xcbinc"))
        .include(format!("{xau}/include")).include(&xp).include(&pers)
        .include(format!("{musl}/include")).include(format!("{musl}/obj/include"))
        .include(format!("{musl}/arch/x86_64")).include(format!("{musl}/arch/generic"))
        .define("HAVE_CONFIG_H", None).define("_GNU_SOURCE", None)
        .define("OXBOW_ARGV0", "\"xlibdemo\"")
        .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
        .flag("-Wno-everything").opt_level(2);
    // libX11: core + xcms + xkb + i18n (minus dlopen/xtrans-only files)
    for f in cfiles(&format!("{x11}/src"), &[]) { b.file(f); }
    for f in cfiles(&format!("{x11}/src/xcms"), &[]) { b.file(f); }
    for f in cfiles(&format!("{x11}/src/xkb"), &[]) { b.file(f); }
    for f in cfiles(&format!("{x11}/src/xlibi18n"), &["XlcDL.c", "xim_trans.c"]) { b.file(f); }
    // static locale-loader modules (define _XlcGenericLoader/_XlcDefaultLoader/_XlcUtf8Loader)
    for f in ["gen/lcGenConv", "def/lcDefConv", "Utf8/lcUTF8Load"] {
        b.file(format!("{x11}/modules/lc/{f}.c"));
    }
    // output/input method modules (define _XInitOM/_XInitIM; minus the xtrans IM transport)
    for f in cfiles(&format!("{x11}/modules/om/generic"), &[]) { b.file(f); }
    for f in cfiles(&format!("{x11}/modules/im/ximcp"), &["imTrans.c"]) { b.file(f); }
    // libxcb + libXau (the transport)
    for f in ["xcb_conn","xcb_out","xcb_in","xcb_ext","xcb_xid","xcb_list","xcb_util","xcb_auth","xproto","bigreq","xc_misc"] {
        b.file(format!("{xcb}/src/{f}.c"));
    }
    for f in ["AuDispose","AuFileName","AuGetAddr","AuGetBest","AuLock","AuRead","AuUnlock","AuWrite"] {
        b.file(format!("{xau}/{f}.c"));
    }
    // client + personality
    b.file("xlibdemo.c")
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"));
    b.compile("xlibdemoprog");
}
