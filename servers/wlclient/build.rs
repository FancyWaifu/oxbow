// wlclient — a standalone Wayland client. Builds libwayland (client side) from
// the oxwl crate + libffi from oxffi + the client main, via the C-port harness.
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
        .include("../oxwl/wl-include")
        .include("../oxffi/ffi-include")
        .include("../../libc/include")
        .include("../oxwl")
        .define("HAVE_CONFIG_H", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    for f in [
        "../oxwl/wl-src/wayland-util.c",
        "../oxwl/wl-src/connection.c",
        "../oxwl/wl-src/wayland-os.c",
        "../oxwl/wl-src/wayland-protocol.c",
        "../oxwl/wl-src/wayland-client.c",
    ] {
        b.file(f);
    }
    for f in [
        "../oxffi/ffi-src/prep_cif.c",
        "../oxffi/ffi-src/types.c",
        "../oxffi/ffi-src/raw_api.c",
        "../oxffi/ffi-src/x86/ffi64.c",
        "../oxffi/ffi-src/x86/ffiw64.c",
        "../oxffi/ffi-src/x86/unix64.S",
        "../oxffi/ffi-src/x86/win64.S",
    ] {
        b.file(f);
    }
    b.file("src/oxmain.c");
    b.compile("wlclient");
}
