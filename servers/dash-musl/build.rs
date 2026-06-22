// Build dash (the Debian Almquist shell) as a musl-linked oxbow program — the Phase
// 10 "real /bin/sh" port. dash source is out-of-repo at ~/musl-oxbow/dash, where its
// autotools build was run ONCE on the host to generate config.h + the codegen outputs
// (builtins.c, init.c, nodes.c, signames.c, syntax.c, token.h). Here we cross-compile
// dash's C against musl + the oxbow personality and link the prebuilt musl libc.a.
//
// Job control is disabled (-DJOBS=0): oxbow has no process groups / terminal pgrp.
use std::path::Path;
use std::process::Command;

// dash translation units, minus the host-only codegen tools (mk*.c).
const DASH_SRC: &[&str] = &[
    "alias", "arith_yacc", "arith_yylex", "builtins", "cd", "error", "eval", "exec",
    "expand", "histedit", "init", "input", "jobs", "mail", "main", "memalloc",
    "miscbltin", "mystring", "nodes", "options", "output", "parser", "redir", "show",
    "signames", "syntax", "system", "trap", "var",
];

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let dash = format!("{home}/musl-oxbow/dash");

    assert!(
        Path::new(&format!("{dash}/src/builtins.c")).exists(),
        "dash codegen missing at {dash}/src — run its host build first (autogen+configure+make)"
    );
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={dash}/src");

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
        .include(&dash)        // config.h
        .include(format!("{dash}/src")) // dash headers + generated token.h etc.
        .include(format!("{musl}/include"))
        .include(format!("{musl}/obj/include"))
        .include(format!("{musl}/arch/x86_64"))
        .include(format!("{musl}/arch/generic"))
        // dash's build force-includes config.h and sets HAVE_CONFIG_H.
        .flag("-include")
        .flag(&format!("{dash}/config.h"))
        .define("HAVE_CONFIG_H", None)
        .define("SHELL", None) // build as the shell, not standalone bltin tools
        // the oxbow personality: crt bridge + Linux-syscall dispatcher.
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"));
    for f in DASH_SRC {
        b.file(format!("{dash}/src/{f}.c"));
    }
    // the echo/printf/test/times builtins live under bltin/.
    for f in ["printf", "test", "times"] {
        b.file(format!("{dash}/src/bltin/{f}.c"));
    }
    b.include(format!("{dash}/src/bltin"));
    b.flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .define("OXBOW_ARGV0", "\"sh\"")
        .opt_level(2)
        .compile("dashprog");
}
