// Build onetrueawk (BWK awk) as a musl-linked oxbow program — the Phase 5 "real
// upstream app" port. The awk sources live OUT of the repo at ~/musl-oxbow/onetrueawk
// (fetched, like the vendored musl). We generate awk's parser tables on the HOST
// (bison + maketab), then cross-compile awk's C against musl's headers + the oxbow
// personality (crt bridge + syscall dispatcher) and link the prebuilt musl libc.a.
use std::path::Path;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let pers = format!("{dir}/../../userland/musl-personality");
    let home = std::env::var("HOME").unwrap();
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let awk = format!("{home}/musl-oxbow/onetrueawk");

    assert!(Path::new(&awk).exists(), "onetrueawk not found at {awk} — clone it first");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={awk}");

    // --- host codegen: bison grammar + maketab's proctab.c (run in the awk dir) ---
    if !Path::new(&format!("{awk}/awkgram.tab.c")).exists() {
        run(Command::new("bison").args(["-d", "-o", "awkgram.tab.c", "awkgram.y"]).current_dir(&awk));
    }
    if !Path::new(&format!("{awk}/proctab.c")).exists() {
        run(Command::new("clang").args(["-O2", "-w", "maketab.c", "-o", "maketab"]).current_dir(&awk));
        let out = Command::new("./maketab")
            .arg("awkgram.tab.h")
            .current_dir(&awk)
            .output()
            .expect("maketab failed");
        std::fs::write(format!("{awk}/proctab.c"), out.stdout).unwrap();
    }

    // musl libc.a (with libm folded in) — grouped with our objects so cross-refs
    // (crt -> __libc_start_main, musl -> __oxbow_syscall) resolve.
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
        .include(&awk) // awk.h, proto.h, awkgram.tab.h
        .include(format!("{musl}/include"))
        .include(format!("{musl}/obj/include"))
        .include(format!("{musl}/arch/x86_64"))
        .include(format!("{musl}/arch/generic"))
        // oxbow personality: crt bridge + Linux-syscall dispatcher.
        .file(format!("{pers}/crt_glue.c"))
        .file(format!("{pers}/oxbow_syscall.c"));
    // awk's own translation units (maketab.c is host-only; excluded).
    for f in ["b", "lex", "lib", "main", "parse", "run", "tran", "awkgram.tab", "proctab"] {
        b.file(format!("{awk}/{f}.c"));
    }
    b.flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .define("OXBOW_ARGV0", "\"awk\"")
        .define("HAS_ISBLANK", "1")
        .opt_level(2)
        .compile("awkprog");
}

fn run(cmd: &mut Command) {
    let st = cmd.status().expect("spawn failed");
    assert!(st.success(), "command failed: {cmd:?}");
}
