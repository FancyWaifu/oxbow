use std::process::Command;

// Two jobs:
//  1. Apply the USER link layout to this crate (a low-half ET_EXEC at 0x200000),
//     independent of the kernel's higher-half linker script.
//  2. Compile the embedded Lua 5.4 interpreter — the SAME C sources servers/lua
//     uses (shared from ../lua/lua), plus a small glue layer (csrc/luaglue.c) —
//     into the shell binary, so the shell can run Lua control flow in-process.
//
// Output routing: Lua's `print`/`io.write` macros (lua_writestring / lua_writeline
// / lua_writestringerror in luaconf.h) normally hit libc's stdout FILE. But the
// shell links libc with `default-features = false`, so that FILE is never set up.
// We override those macros on the command line to call our glue (ox_lua_write),
// which forwards to the shell's tty path (TAG_TTY_WRITE) via a Rust callback —
// the one console the shell actually owns.
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
    // dynamic loading — identical to servers/lua/build.rs (those .c files are the
    // single shared copy under servers/lua/lua). Excluded: lua.c/luac.c/linit.c
    // (their own mains + openlibs — our csrc/luaglue.c provides these),
    // liolib/loslib/loadlib (file I/O, os, dlopen), lmathlib (transcendentals),
    // ldblib (debug).
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
        .include("../lua/lua") // shared Lua C sources/headers
        .include("../../libc/include")
        .include("csrc") // ox_lua_io.h (the write-callback declarations)
        .define("LUA_USE_C89", None) // generic ANSI-C path: no POSIX/dlopen/locale
        // Route Lua's REPL/print output through the shell's tty instead of a libc
        // stdout FILE the shell never initializes. ox_lua_write/ox_lua_writeerr
        // are declared in csrc/ox_lua_io.h (force-included below) and implemented
        // in csrc/luaglue.c.
        .define("lua_writestring(s,l)", Some("ox_lua_write((const char*)(s),(l))"))
        .define("lua_writeline()", Some("ox_lua_write(\"\\n\",1)"))
        .define("lua_writestringerror(s,p)", Some("ox_lua_writeerr((s))"))
        .flag("-include")
        .flag("csrc/ox_lua_io.h")
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration")
        .flag("-Wno-everything")
        .opt_level(2);
    for f in core.iter().chain(libs.iter()) {
        b.file(format!("../lua/lua/{f}.c"));
        println!("cargo:rerun-if-changed=../lua/lua/{f}.c");
    }
    b.file("csrc/luaglue.c");
    println!("cargo:rerun-if-changed=csrc/luaglue.c");
    println!("cargo:rerun-if-changed=csrc/ox_lua_io.h");
    b.compile("luashell");
}
