// Build vendored FreeType (§51) for oxbow: the glyph rasterizer for the terminal.
// Minimal module set (TrueType + sfnt + psnames + smooth/mono renderers +
// autofit), built with FreeType's "smush" single-file units. ftstdlib.h maps
// straight to our libc; setjmp/longjmp + qsort already exist. We load fonts from
// memory (FT_New_Memory_Face), so the on-disk/stream paths are linked-not-run.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
    println!("cargo:rerun-if-changed=src/oxmain.c");

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
        .include("ft/include")
        .include("../../libc/include")
        .define("FT2_BUILD_LIBRARY", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    for f in [
        "ft/src/base/ftsystem.c",
        "ft/src/base/ftinit.c",
        "ft/src/base/ftbase.c",
        "ft/src/base/ftbbox.c",
        "ft/src/base/ftbitmap.c",
        "ft/src/base/ftglyph.c",
        "ft/src/base/ftdebug.c",
        "ft/src/base/ftmm.c",
        "ft/src/gzip/ftgzip.c",
        "ft/src/sfnt/sfnt.c",
        "ft/src/truetype/truetype.c",
        "ft/src/smooth/smooth.c",
        "ft/src/psnames/psnames.c",
        "ft/src/autofit/autofit.c",
        "ft/src/raster/raster.c",
    ] {
        println!("cargo:rerun-if-changed={f}");
        b.file(f);
    }
    b.file("src/oxmain.c");
    b.compile("freetype");
}
