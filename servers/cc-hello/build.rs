// Apply the user link layout AND compile the C program (src/hello.c) with clang
// for the bare target, linking it into this Rust binary. The Rust side provides
// `_start` (via oxbow-rt) + the libc functions; the C side provides `main`.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
    println!("cargo:rerun-if-changed=src/hello.c");

    // The cross-compiled C object is ELF, so the static archive must be built by
    // LLVM's `ar` (Apple's chokes on ELF). llvm-ar ships with the rustup
    // `llvm-tools` component; find it under the toolchain sysroot.
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    )
    .unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);

    cc::Build::new()
        .compiler("clang")
        .archiver(&llvm_ar)
        .file("src/hello.c")
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .opt_level(2)
        .compile("cprog");
}
