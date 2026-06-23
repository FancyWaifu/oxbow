// Build emikulic's darkhttpd — a real, unmodified upstream static web server — as a
// musl-linked oxbow program. The capstone of the socket personality (client TCP, UDP+DNS,
// server TCP): a genuine network daemon people deploy, compiled straight from upstream C
// against musl + the oxbow personality. Source is out-of-repo at ~/musl-oxbow/darkhttpd
// (cloned from github.com/emikulic/darkhttpd), like musl/awk/kilo/dash.
//
// Single darkhttpd.c. Because we compile with --target=x86_64-unknown-none, neither
// __linux nor __sun__ is defined, so darkhttpd takes its PORTABLE paths: the read()-based
// sendfile fallback (no sendfile syscall) and no Linux-only headers. We DO define
// _GNU_SOURCE so musl exposes vasprintf()/strsignal() that darkhttpd uses unconditionally.
use std::path::Path;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let dark = format!("{home}/musl-oxbow/darkhttpd");

    assert!(
        Path::new(&dark).exists(),
        "darkhttpd not found at {dark} — clone emikulic/darkhttpd first"
    );
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={dark}");

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
        .file(format!("{dark}/darkhttpd.c"))
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .define("OXBOW_ARGV0", "\"darkhttpd\"")
        .define("_GNU_SOURCE", None)
        .define("_POSIX_C_SOURCE", "200809L")
        .opt_level(2)
        .compile("darkhttpdprog");
}
