// wlclient — a standalone Wayland client. §96 Phase 4: oxui + libwayland + libffi link
// dynamically from /lib/liboxui.so (shared helper); wlclient statically links only
// libxkbcommon + its simple-shm.c app.
use std::process::Command;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../oxui/dynlink.rs"));

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    emit_oxui_dynlink(dir); // §96 Phase 4: link /lib/liboxui.so dynamically
    println!("cargo:rerun-if-changed=src/simple-shm.c");

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

    // libxkbcommon (§48) needs its OWN config.h, which would collide with the
    // client/wayland config.h if compiled in the same unit — so build it as a
    // separate archive whose include path puts xkb/config.h first.
    let mut x = cc::Build::new();
    x.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include("../oxxkb/xkb") // xkb config.h
        .include("../oxxkb/xkb/include")
        .include("../oxxkb/xkb/src")
        .include("../oxxkb/xkb/src/xkbcomp")
        .include("../../libc/include")
        .define("HAVE_CONFIG_H", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);
    for f in [
        "atom", "context", "context-priv", "keymap", "keymap-priv", "keysym",
        "keysym-utf", "state", "text", "utf8", "util-list", "utils",
    ] {
        x.file(format!("../oxxkb/xkb/src/{f}.c"));
    }
    for f in [
        "action", "ast-build", "compat", "expr", "include", "keycodes", "keymap",
        "keymap-dump", "keywords", "parser", "rules", "scanner", "symbols", "types",
        "vmod", "xkbcomp",
    ] {
        x.file(format!("../oxxkb/xkb/src/xkbcomp/{f}.c"));
    }
    x.compile("xkbcommon");

    // --- FreeType archive: §96 Phase 4 — the SHARED /lib/liboxui.so bundles oxui_text,
    // which uses FreeType, so EVERY consumer must provide FT_* (even though wlclient
    // itself draws no text). ---
    let mut ft = cc::Build::new();
    ft.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include("../../libc/include")
        .include("../oxft/ft/include")
        .define("FT2_BUILD_LIBRARY", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);
    for f in [
        "base/ftsystem", "base/ftinit", "base/ftbase", "base/ftbbox", "base/ftbitmap",
        "base/ftglyph", "base/ftdebug", "base/ftmm", "gzip/ftgzip", "sfnt/sfnt",
        "truetype/truetype", "smooth/smooth", "psnames/psnames", "autofit/autofit",
        "raster/raster",
    ] {
        ft.file(format!("../oxft/ft/src/{f}.c"));
    }
    ft.compile("freetype");

    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc")
        .flag("-isystem")
        .flag(&res_inc)
        .include("include") // weston shims: config.h, shared/, libweston/, linux/
        .include("../oxwl/wl-include")
        .include("../oxffi/ffi-include")
        .include("../oxxkb/xkb/include") // xkbcommon.h for the client (§48)
        .include("../oxui/include") // oxui.h (§64 toolkit)
        .include("../../libc/include")
        .include("../oxwl")
        .define("HAVE_CONFIG_H", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    // §96 Phase 4: wayland + ffi + oxui.c are in /lib/liboxui.so now.
    b.file("src/simple-shm.c");
    b.compile("wlclient");
}
