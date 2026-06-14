// fsd — the lwext4-backed filesystem server. Builds the vendored lwext4 (ext2,
// BSD-clean subset, shared with the fsext crate at ../fsext/lwext4) plus the
// block-device glue into a static lib against oxbow-libc, for the oxbow target.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");

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

    let lwext4 = "../fsext/lwext4";
    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include(format!("{lwext4}/include"))
        .include("../../libc/include")
        .define("CONFIG_USE_DEFAULT_CFG", "1")
        .define("CONFIG_EXT_FEATURE_SET_LVL", "2") // F_SET_EXT2
        .define("CONFIG_JOURNALING_ENABLE", "0")
        .define("CONFIG_XATTR_ENABLE", "0")
        .define("CONFIG_EXTENTS_ENABLE", "0")
        .define("CONFIG_DEBUG_PRINTF", "0")
        .define("CONFIG_DEBUG_ASSERT", "0")
        .define("CONFIG_HAVE_OWN_ERRNO", "0")
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration")
        .flag("-Wno-everything")
        .opt_level(2);

    for ent in std::fs::read_dir(format!("{lwext4}/src")).unwrap() {
        let path = ent.unwrap().path();
        if path.extension().map(|e| e == "c").unwrap_or(false) {
            println!("cargo:rerun-if-changed={}", path.display());
            b.file(&path);
        }
    }
    b.file("src/blockdev_glue.c");
    println!("cargo:rerun-if-changed=src/blockdev_glue.c");
    b.compile("lwext4_fsd");
}
