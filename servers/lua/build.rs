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

    // The Lua core VM + the libraries that don't need a filesystem, a clock, or
    // dynamic loading. Excluded: lua.c/luac.c (their own mains — we provide
    // oxmain.c), linit.c (custom openlibs in oxmain.c), liolib/loslib/loadlib
    // (file I/O, os, dlopen), lmathlib (transcendentals — the core's // % ^ only
    // need floor/fmod/pow), ldblib (debug).
    let core = [
        "lapi", "lcode", "lctype", "ldebug", "ldo", "ldump", "lfunc", "lgc", "llex", "lmem",
        "lobject", "lopcodes", "lparser", "lstate", "lstring", "ltable", "ltm", "lundump", "lvm",
        "lzio",
    ];
    let libs = ["lauxlib", "lbaselib", "lcorolib", "lstrlib", "ltablib", "lutf8lib"];

    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include("lua")
        .include("../../libc/include")
        .define("LUA_USE_C89", None) // generic ANSI-C path: no POSIX/dlopen/locale
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration")
        .flag("-Wno-everything")
        .opt_level(2);
    for f in core.iter().chain(libs.iter()) {
        b.file(format!("lua/{f}.c"));
        println!("cargo:rerun-if-changed=lua/{f}.c");
    }
    b.file("src/oxmain.c");
    println!("cargo:rerun-if-changed=src/oxmain.c");
    b.compile("lua");
}
