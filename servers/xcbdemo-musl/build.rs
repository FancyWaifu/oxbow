// Build a minimal X client on REAL libxcb (Phase A1 of docs/roadmap.md). Compiles
// libxcb (core + generated xproto/bigreq/xc_misc) + libXau + the xcb demo client +
// the musl personality, linked against musl libc.a. Sources out-of-repo at ~/musl-oxbow
// (libxcb-1.16, xcb-proto-1.16.0, libXau, xorgproto). The protocol C files are generated
// by xcb-proto's c_client.py at build time if missing.
use std::path::Path;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let mo = format!("{home}/musl-oxbow");
    let musl = format!("{mo}/musl-1.2.5");
    let xcb = format!("{mo}/libxcb-1.16");
    let xcbproto = format!("{mo}/xcb-proto-1.16.0");
    let xau = format!("{mo}/libXau");
    let xp = format!("{mo}/xorgproto/include");
    let out = std::env::var("OUT_DIR").unwrap();

    assert!(Path::new(&xcb).exists(), "libxcb not found at {xcb}");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed=xcbdemo.c");
    println!("cargo:rerun-if-changed=build.rs");

    // 1. Generate the core protocol C files from xcb-proto XML if missing.
    for p in ["xproto", "bigreq", "xc_misc"] {
        let cfile = format!("{xcb}/src/{p}.c");
        if !Path::new(&cfile).exists() {
            let st = Command::new("python3")
                .current_dir(format!("{xcb}/src"))
                .args(["./c_client.py", "-c", "libxcb 1.16", "-l", "X Version 11",
                       "-s", "3", "-p", &xcbproto, &format!("{xcbproto}/src/{p}.xml")])
                .status()
                .expect("run c_client.py");
            assert!(st.success(), "c_client.py failed for {p}");
        }
    }

    // 2. Stage the xcb headers under an `xcb/` dir so the client's <xcb/xcb.h> resolves.
    let xcbinc = format!("{out}/xcbinc/xcb");
    std::fs::create_dir_all(&xcbinc).unwrap();
    for h in ["xcb", "xcbext", "xproto", "bigreq", "xc_misc"] {
        std::fs::copy(format!("{xcb}/src/{h}.h"), format!("{xcbinc}/{h}.h")).unwrap();
    }

    // 3. A hand-written config.h for libxcb (in its src dir already; ensure it exists).
    let cfg = format!("{xcb}/src/config.h");
    if !Path::new(&cfg).exists() {
        std::fs::write(&cfg,
            "#define HAVE_GETADDRINFO 1\n#define HAVE_SENDMSG 1\n#define XCB_QUEUE_BUFFER_SIZE 16384\n").unwrap();
    }

    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");

    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout).unwrap();
    let res_inc = format!("{}/include", res.trim());

    let mut b = cc::Build::new();
    b.compiler("clang").archiver(&llvm_ar)
        .flag("-nostdinc").flag("-isystem").flag(&res_inc)
        .include(format!("{xcb}/src"))           // libxcb internal headers + config.h
        .include(format!("{out}/xcbinc"))        // <xcb/xcb.h> for the client
        .include(format!("{xau}/include"))       // X11/Xauth.h (xcb_auth)
        .include(&xp)                            // xorgproto (X11/Xproto, keysymdef)
        .include(&pers)
        .include(format!("{musl}/include")).include(format!("{musl}/obj/include"))
        .include(format!("{musl}/arch/x86_64")).include(format!("{musl}/arch/generic"))
        .define("HAVE_CONFIG_H", None).define("_GNU_SOURCE", None)
        .define("OXBOW_ARGV0", "\"xcbdemo\"")
        .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
        .flag("-Wno-everything").opt_level(2);
    // libxcb core + generated protocol
    for f in ["xcb_conn", "xcb_out", "xcb_in", "xcb_ext", "xcb_xid", "xcb_list",
              "xcb_util", "xcb_auth", "xproto", "bigreq", "xc_misc"] {
        b.file(format!("{xcb}/src/{f}.c"));
    }
    // libXau (auth lookup; returns NULL with no .Xauthority -> no-auth connect)
    for f in ["AuDispose", "AuFileName", "AuGetAddr", "AuGetBest",
              "AuLock", "AuRead", "AuUnlock", "AuWrite"] {
        b.file(format!("{xau}/{f}.c"));
    }
    // the client + the musl personality
    b.file("xcbdemo.c")
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"));
    b.compile("xcbdemoprog");
}
