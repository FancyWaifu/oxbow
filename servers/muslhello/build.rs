// Build a musl-linked oxbow program: compile the crt bridge + the personality
// syscall dispatcher + the test main against musl's headers, and link them with
// the prebuilt musl libc.a (see userland/musl-personality/build-musl.sh) using the
// musl-aware linker script. oxbow-rt supplies _start; crt_glue supplies oxbow_main.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");

    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    // Link musl's static libc, grouped with our objects so the cross-references
    // (crt -> __libc_start_main, musl -> __oxbow_syscall) all resolve.
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");

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

    // One build: the dispatcher + crt use our headers; the test main uses musl's.
    // All three see both include sets (no conflicts), so a single config works.
    cc::Build::new()
        .compiler("clang")
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include(&pers)
        .include(format!("{musl}/include"))
        .include(format!("{musl}/obj/include")) // generated headers (bits/alltypes.h)
        .include(format!("{musl}/arch/x86_64"))
        .include(format!("{musl}/arch/generic"))
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"))
        .file(format!("{pers}/muslhello.c"))
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .define("OXBOW_ARGV0", "\"muslhello\"")
        .opt_level(2)
        .compile("muslprog");
}
