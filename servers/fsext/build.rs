// Build the vendored lwext4 (BSD-clean ext2 subset) + the fstest demo into a
// static lib, against oxbow-libc, for the oxbow user target. Mirrors the
// lua/tcc C-port harness. The two GPLv2 files (ext4_extent.c, ext4_xattr.c) were
// never vendored; ext2 needs neither.
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

    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include("lwext4/include")
        .include("../../libc/include")
        // Configure for ext2: no journaling, extents, or xattr (so the two GPLv2
        // files are not needed) and quiet the internal debug printf.
        // USE_DEFAULT_CFG=1 takes the inline #ifndef defaults (which these -D
        // flags override) instead of a CMake-generated config header.
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

    // Every vendored lwext4 source (the BSD-clean subset).
    let dir_iter = std::fs::read_dir("lwext4/src").unwrap();
    for ent in dir_iter {
        let path = ent.unwrap().path();
        if path.extension().map(|e| e == "c").unwrap_or(false) {
            println!("cargo:rerun-if-changed={}", path.display());
            b.file(&path);
        }
    }
    b.file("src/oxmain.c");
    println!("cargo:rerun-if-changed=src/oxmain.c");
    b.compile("lwext4");
}
