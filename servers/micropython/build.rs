use std::fs;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");

    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    )
    .unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout,
    )
    .unwrap();
    let res_inc = format!("{}/include", res.trim());

    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        // -I mp: "py/...", "shared/...", "extmod/..."; gen: "genhdr/..."; port:
        // "mpconfigport.h"/"mphalport.h"; libc headers last.
        .include("mp")
        .include("gen")
        .include("port")
        .include("../../libc/include")
        .define("NDEBUG", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration")
        .flag("-Wno-everything")
        .opt_level(2);

    // The whole py/ core (most arch-specific files compile to nothing on x86_64).
    for entry in fs::read_dir("mp/py").unwrap() {
        let p = entry.unwrap().path();
        if p.extension().map(|e| e == "c").unwrap_or(false) {
            println!("cargo:rerun-if-changed={}", p.display());
            b.file(&p);
        }
    }
    // Support + port files. NOT shared/libc/printf.c or string0.c — oxbow-libc
    // already provides printf/memcpy/etc. (would be duplicate symbols).
    for f in [
        "mp/shared/runtime/stdout_helpers.c",
        "port/oxhal.c",
        "port/oxmain.c",
        "port/_frozen_mpy.c",
    ] {
        println!("cargo:rerun-if-changed={f}");
        b.file(f);
    }
    b.compile("micropython");
}
