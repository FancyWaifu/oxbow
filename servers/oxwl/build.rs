// Build vendored libwayland (1.22.0) for oxbow. Step 4 of the Wayland road. The
// wire core (wayland-util + connection + os + generated protocol) plus libffi
// (compiled from ../oxffi, since connection.c marshals via ffi_call). The
// server/client libs + event loop come in later steps. Mirrors the c-ares/lwext4
// C-port harness; OS couplings (epoll/mmap/memfd) are headed/stubbed in libc.
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
        .include("wl-include")
        .include("../oxffi/ffi-include") // ffi.h for connection.c marshalling
        .include("../../libc/include")
        .define("HAVE_CONFIG_H", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    // libwayland wire core + generated protocol.
    for f in [
        "wl-src/wayland-util.c",
        "wl-src/connection.c",
        "wl-src/wayland-os.c",
        "wl-src/wayland-protocol.c",
        "wl-src/event-loop.c",
        "wl-src/wayland-server.c",
        "wl-src/wayland-client.c",
    ] {
        println!("cargo:rerun-if-changed={f}");
        b.file(f);
    }
    // libffi (from the sibling crate) — connection.c calls ffi_call.
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
    println!("cargo:rerun-if-changed=src/wl_server_side.c");
    b.file("src/wl_server_side.c");
    b.file("src/oxmain.c");
    b.compile("wl");
}
