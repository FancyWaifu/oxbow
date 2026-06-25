// sysmon — a system-monitor oxui app. §96 Phase 3/4: oxui + libwayland + libffi live in
// /lib/liboxui.so (linked dynamically via the shared helper below); sysmon statically
// links only libxkbcommon + FreeType + the app, and imports oxui_* at runtime.
use std::process::Command;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../oxui/dynlink.rs"));

fn harness(llvm_ar: &str, res_inc: &str) -> cc::Build {
    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(res_inc)
        .include("../../libc/include")
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);
    b
}

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");

    // §96 Phase 3: sysmon links oxui DYNAMICALLY from /lib/liboxui.so instead of
    // baking oxui.c + oxui_text.c into its own binary. Build the .so first so the
    // linker can resolve sysmon.c's oxui_* calls against it (DT_NEEDED liboxui.so).
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    )
    .unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);

    // §96 Phase 3/4: link oxui dynamically from /lib/liboxui.so (shared helper).
    emit_oxui_dynlink(dir);
    println!("cargo:rerun-if-changed=src/sysmon.c");
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout,
    )
    .unwrap();
    let res_inc = format!("{}/include", res.trim());

    // --- libxkbcommon archive (its own config.h) ---
    let mut x = harness(&llvm_ar, &res_inc);
    x.include("../oxxkb/xkb")
        .include("../oxxkb/xkb/include")
        .include("../oxxkb/xkb/src")
        .include("../oxxkb/xkb/src/xkbcomp")
        .define("HAVE_CONFIG_H", None);
    for f in ["atom","context","context-priv","keymap","keymap-priv","keysym",
              "keysym-utf","state","text","utf8","util-list","utils"] {
        x.file(format!("../oxxkb/xkb/src/{f}.c"));
    }
    for f in ["action","ast-build","compat","expr","include","keycodes","keymap",
              "keymap-dump","keywords","parser","rules","scanner","symbols","types",
              "vmod","xkbcomp"] {
        x.file(format!("../oxxkb/xkb/src/xkbcomp/{f}.c"));
    }
    x.compile("xkbcommon");

    // --- FreeType archive (for oxui_text) ---
    let mut ft = harness(&llvm_ar, &res_inc);
    ft.include("../oxft/ft/include").define("FT2_BUILD_LIBRARY", None);
    for f in ["base/ftsystem","base/ftinit","base/ftbase","base/ftbbox",
              "base/ftbitmap","base/ftglyph","base/ftdebug","base/ftmm",
              "gzip/ftgzip","sfnt/sfnt","truetype/truetype","smooth/smooth",
              "psnames/psnames","autofit/autofit","raster/raster"] {
        ft.file(format!("../oxft/ft/src/{f}.c"));
    }
    ft.compile("freetype");

    // --- main unit: just sysmon.c. §96 Phase 3: oxui.c + oxui_text.c AND wayland + ffi
    // all live in /lib/liboxui.so now (wayland's wl_*_interface are DATA that a non-PIE
    // exe can't export to the .so, so wayland — which defines+uses them — is bundled in
    // the .so, and ffi comes with it). xkb + FreeType stay static-in-exe (the .so imports
    // only their FUNCTIONS). sysmon.c's oxui_* calls are JUMP_SLOTs resolved at runtime. ---
    let mut b = harness(&llvm_ar, &res_inc);
    b.include("../oxterm/include")
        .include("../oxxkb/xkb/include")
        .include("../oxft/ft/include")
        .include("../oxui/include")
        .define("HAVE_CONFIG_H", None);
    b.file("src/sysmon.c");
    b.compile("sysmon");
}
