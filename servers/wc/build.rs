// Build an sbase tool (the crate name selects the tool source) for oxbow: compile
// the verbatim sbase .c + the lean port support against oxbow-libc, linked with the
// user layout. oxbow-rt supplies `_start`/`oxbow_main`; the C side supplies `main`.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let sb = format!("{dir}/../../userland/sbase");
    println!("cargo:rustc-link-arg=-T{sb}/user.ld");
    println!("cargo:rerun-if-changed={sb}");

    // Cross-compiled C is ELF, so the static archive needs LLVM's ar (Apple's chokes).
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    )
    .unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let compiler = std::env::var("CC").unwrap_or_else(|_| "clang".to_string());
    // Clang's own resource headers (stddef.h/stdarg.h/limits.h) — oxbow-libc provides
    // the rest. -nostdinc keeps the host's libc headers out.
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout,
    )
    .unwrap();
    let res_inc = format!("{}/include", res.trim());

    let tool = std::env::var("CARGO_PKG_NAME").unwrap();
    let mut build = cc::Build::new();
    build
        .compiler(&compiler)
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include(&sb)
        .include(format!("{dir}/../../libc/include"))
        .file(format!("{sb}/{tool}.c"))
        .file(format!("{sb}/getline.c"))
        .file(format!("{sb}/oxcompat.c"))
        .file(format!("{sb}/re.c"));
    // Vendor the real sbase libutf (UTF-8) + libutil wholesale — glob so new
    // support files are picked up without touching this template.
    for sub in ["libutf", "libutil"] {
        for ent in std::fs::read_dir(format!("{sb}/{sub}")).unwrap() {
            let p = ent.unwrap().path();
            if p.extension().map(|e| e == "c").unwrap_or(false) {
                build.file(p);
            }
        }
    }
    build
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2)
        .compile("cprog");
}
