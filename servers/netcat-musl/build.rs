// Build GNU netcat 0.7.1 — the classic TCP/UDP swiss-army tool — as a musl-linked
// oxbow program. A second real upstream network tool (after darkhttpd) exercising the
// CLIENT path (getaddrinfo/connect) and the select() multiplex of stdin<->socket.
// Source is out-of-repo at ~/musl-oxbow/netcat-0.7.1 (GNU netcat tarball). Autotools-
// based like dash: instead of a Darwin host configure, we hand-wrote a Linux/musl
// config.h (in the source dir) and force-include it. _GNU_SOURCE for stpcpy/mempcpy.
use std::path::Path;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let nc = format!("{home}/musl-oxbow/netcat-0.7.1");

    assert!(Path::new(&nc).exists(), "netcat not found at {nc} — extract netcat-0.7.1 first");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={nc}");

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

    let mut b = cc::Build::new();
    b.compiler("clang")
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include(&pers)
        .include(&nc) // hand-written config.h
        .include(format!("{nc}/src")) // netcat.h
        .include(format!("{musl}/include"))
        .include(format!("{musl}/obj/include"))
        .include(format!("{musl}/arch/x86_64"))
        .include(format!("{musl}/arch/generic"))
        .flag("-include")
        .flag(&format!("{nc}/config.h"))
        .define("HAVE_CONFIG_H", None)
        .define("_GNU_SOURCE", None)
        .define("OXBOW_ARGV0", "\"netcat\"")
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"));
    for f in ["core", "flagset", "misc", "netcat", "network", "telnet", "udphelper"] {
        b.file(format!("{nc}/src/{f}.c"));
    }
    b.flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2)
        .compile("netcatprog");
}
