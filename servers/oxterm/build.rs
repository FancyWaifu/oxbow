// oxterm — the Wayland terminal (§52). Links the whole ported stack: libwayland
// (client) + libffi + libxkbcommon (keys) + libvterm (screen model) + FreeType
// (glyphs). xkb/vterm/freetype are each compiled as their OWN archive so their
// private headers + config.h don't collide; the main unit (wayland + ffi +
// term.c) sees only their PUBLIC headers. §96 Phase 4: oxui + libwayland + libffi link
// dynamically from /lib/liboxui.so (shared helper); oxterm statically links only
// libxkbcommon + libvterm + FreeType + term.c.
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
    emit_oxui_dynlink(dir); // §96 Phase 4: link /lib/liboxui.so dynamically
    println!("cargo:rerun-if-changed=src/term.c");

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

    // --- libxkbcommon archive ---
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

    // --- libvterm archive ---
    let mut v = harness(&llvm_ar, &res_inc);
    v.include("../oxvterm/vterm/include").include("../oxvterm/vterm/src");
    for f in ["encoding","keyboard","mouse","parser","pen","screen","state",
              "unicode","vterm"] {
        v.file(format!("../oxvterm/vterm/src/{f}.c"));
    }
    v.compile("vterm");

    // --- FreeType archive ---
    let mut ft = harness(&llvm_ar, &res_inc);
    ft.include("../oxft/ft/include").define("FT2_BUILD_LIBRARY", None);
    for f in ["base/ftsystem","base/ftinit","base/ftbase","base/ftbbox",
              "base/ftbitmap","base/ftglyph","base/ftdebug","base/ftmm",
              "gzip/ftgzip","sfnt/sfnt","truetype/truetype","smooth/smooth",
              "psnames/psnames","autofit/autofit","raster/raster"] {
        ft.file(format!("../oxft/ft/src/{f}.c"));
    }
    ft.compile("freetype");

    // --- main unit: wayland (client) + ffi + term.c ---
    let mut b = harness(&llvm_ar, &res_inc);
    b.include("include") // weston shims: config.h, ...
        .include("../oxwl/wl-include")
        .include("../oxffi/ffi-include")
        .include("../oxxkb/xkb/include")
        .include("../oxvterm/vterm/include")
        .include("../oxft/ft/include")
        .include("../oxui/include") // §64: the UI toolkit
        .include("font")
        .include("../oxwl")
        .define("HAVE_CONFIG_H", None);
    // §96 Phase 4: wayland + ffi + oxui.c are in /lib/liboxui.so now.
    b.file("src/term.c");
    b.compile("oxterm");
}
