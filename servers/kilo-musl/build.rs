// Build antirez's kilo editor as a musl-linked oxbow program — the Phase 7 "real
// interactive TUI app" port. kilo source is out-of-repo at ~/musl-oxbow/kilo (cloned,
// like musl + awk). Single .c; cross-compile against musl's headers + the oxbow
// personality (crt bridge + syscall dispatcher) and link the prebuilt musl libc.a.
use std::path::Path;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let kilo = format!("{home}/musl-oxbow/kilo");

    assert!(Path::new(&kilo).exists(), "kilo not found at {kilo} — clone antirez/kilo first");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={kilo}");

    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
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

    cc::Build::new()
        .compiler("clang")
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include(&pers)
        .include(format!("{musl}/include"))
        .include(format!("{musl}/obj/include"))
        .include(format!("{musl}/arch/x86_64"))
        .include(format!("{musl}/arch/generic"))
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"))
        .file(format!("{kilo}/kilo.c"))
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .define("OXBOW_ARGV0", "\"kilo\"")
        .define("_POSIX_C_SOURCE", "200809L")
        .opt_level(2)
        .compile("kiloprog");
}
