// Build vendored libffi (x86_64 SysV) for oxbow: the dynamic-call library
// libwayland uses to dispatch wire messages to typed handlers. We compile the
// portable core (prep_cif, types, raw_api) + the x86_64 backend (ffi64 + ffiw64
// for the EFI64 ABI dispatch) + the unix64/win64 assembly. Closures (executable
// trampolines) are linked but never invoked — libwayland only uses ffi_call, and
// oxbow's W^X has no executable-memory syscall. Mirrors the c-ares/lwext4 harness.
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
        .include("ffi-include")
        .include("../../libc/include")
        .define("HAVE_CONFIG_H", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    for f in [
        "ffi-src/prep_cif.c",
        "ffi-src/types.c",
        "ffi-src/raw_api.c",
        "ffi-src/x86/ffi64.c",
        "ffi-src/x86/ffiw64.c",
        "ffi-src/x86/unix64.S",
        "ffi-src/x86/win64.S",
    ] {
        println!("cargo:rerun-if-changed={f}");
        b.file(f);
    }
    b.file("src/oxmain.c");
    b.compile("ffi");
}
