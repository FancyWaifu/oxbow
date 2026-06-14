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

    let files = ["qjs/quickjs.c", "qjs/cutils.c", "qjs/libregexp.c", "qjs/libunicode.c", "qjs/xsum.c", "src/oxmain.c"];
    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc").flag("-isystem").flag(&res_inc)
        .include("qjs").include("../../libc/include")
        .define("_GNU_SOURCE", None)
        .define("CONFIG_VERSION", "\"oxbow\"")
        .define("NO_TM_GMTOFF", None) // oxbow's struct tm has no tm_gmtoff field
        .define("alloca(x)", "__builtin_alloca(x)") // force the compiler builtin everywhere
        .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration").flag("-Wno-everything")
        .opt_level(2);
    for f in files { b.file(f); println!("cargo:rerun-if-changed={f}"); }
    b.compile("qjs");
}
