use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");

    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    ).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout,
    ).unwrap();
    let res_inc = format!("{}/include", res.trim());

    let files = [
        "tinycc/tcc.c", "tinycc/libtcc.c", "tinycc/tccpp.c", "tinycc/tccgen.c",
        "tinycc/tccelf.c", "tinycc/tccrun.c", "tinycc/tccasm.c", "tinycc/x86_64-gen.c",
        "tinycc/x86_64-link.c", "tinycc/tccdbg.c", "tinycc/i386-asm.c",
    ];
    for f in files { println!("cargo:rerun-if-changed={f}"); }

    cc::Build::new()
        .compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem").flag(&res_inc)
        .include("tinycc")
        .include("../../libc/include")
        .define("TCC_TARGET_X86_64", None)
        .define("ONE_SOURCE", "0")
        .define("CONFIG_TCC_PREDEFS", "1")
        // No POSIX signals on oxbow — disable the crash-backtrace + bounds-check
        // features (they install signal handlers).
        .define("CONFIG_TCC_BACKTRACE", "0")
        .define("CONFIG_TCC_BCHECK", "0")
        .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration").flag("-Wno-int-conversion")
        .flag("-Wno-everything")
        .opt_level(1)
        .files(files)
        .compile("tcc");
}
