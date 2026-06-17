// sysmon — a system-monitor oxui app. Links the client stack: libwayland + libffi
// + libxkbcommon (oxui needs it for keys) + FreeType (oxui_text) + oxui + the app.
use std::process::Command;

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
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
    println!("cargo:rerun-if-changed=src/sysmon.c");
    println!("cargo:rerun-if-changed=../oxui/oxui.c");
    println!("cargo:rerun-if-changed=../oxui/oxui_text.c");

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

    // --- main unit: wayland (client) + ffi + oxui + oxui_text + sysmon.c ---
    let mut b = harness(&llvm_ar, &res_inc);
    b.include("../oxterm/include") // weston shims: config.h, shared/, libweston/
        .include("../oxwl/wl-include")
        .include("../oxffi/ffi-include")
        .include("../oxxkb/xkb/include")
        .include("../oxft/ft/include")
        .include("../oxui/include")
        .include("../oxterm/font") // dejavu_mono.h for oxui_text
        .include("../oxwl")
        .define("HAVE_CONFIG_H", None);
    for f in ["wayland-util","connection","wayland-os","wayland-protocol",
              "xdg-shell-protocol","wayland-client"] {
        b.file(format!("../oxwl/wl-src/{f}.c"));
    }
    for f in ["prep_cif","types","raw_api","x86/ffi64","x86/ffiw64"] {
        b.file(format!("../oxffi/ffi-src/{f}.c"));
    }
    b.file("../oxffi/ffi-src/x86/unix64.S");
    b.file("../oxffi/ffi-src/x86/win64.S");
    b.file("../oxui/oxui.c");
    b.file("../oxui/oxui_text.c");
    b.file("src/sysmon.c");
    b.compile("sysmon");
}
