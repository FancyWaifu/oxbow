// Build vendored libvterm (§50) for oxbow: the terminal state machine — it parses
// escape sequences and maintains the screen grid + scrollback, with no UI, font,
// or PTY of its own (the parts that don't fit oxbow's spawn model). Self-contained
// C, libc-only, no config.h, and the encoding .inc tables ship pre-generated.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
    println!("cargo:rerun-if-changed=src/oxmain.c");

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
        .include("vterm/include")
        .include("vterm/src")
        .include("../../libc/include")
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    for f in [
        "encoding", "keyboard", "mouse", "parser", "pen", "screen", "state",
        "unicode", "vterm",
    ] {
        println!("cargo:rerun-if-changed=vterm/src/{f}.c");
        b.file(format!("vterm/src/{f}.c"));
    }
    b.file("src/oxmain.c");
    b.compile("vterm");
}
