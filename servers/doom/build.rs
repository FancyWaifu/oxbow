// Build DOOM (1993) for oxbow: the doomgeneric engine + an oxui platform layer, linked
// against oxbow-libc and the same graphical client stack as sysmon (libwayland + libffi
// + libxkbcommon + FreeType + oxui). The doomgeneric source is out-of-repo at
// ~/musl-oxbow/doomgeneric (cloned from ozkl/doomgeneric); the platform layer + entry
// shim are in this crate. The engine file list mirrors Makefile.soso (the no-SDL,
// null-sound hobby-OS target), minus its doomgeneric_soso.c platform.
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
    let home = std::env::var("HOME").unwrap();
    let dg = format!("{home}/musl-oxbow/doomgeneric/doomgeneric");
    assert!(
        std::path::Path::new(&dg).exists(),
        "doomgeneric not found at {dg} — clone ozkl/doomgeneric first"
    );
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
    println!("cargo:rerun-if-changed=src/doomgeneric_oxbow.c");
    println!("cargo:rerun-if-changed={dg}");

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

    // --- libxkbcommon (oxui needs it to decode keys) ---
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

    // --- FreeType (oxui_text) ---
    let mut ft = harness(&llvm_ar, &res_inc);
    ft.include("../oxft/ft/include").define("FT2_BUILD_LIBRARY", None);
    for f in ["base/ftsystem","base/ftinit","base/ftbase","base/ftbbox",
              "base/ftbitmap","base/ftglyph","base/ftdebug","base/ftmm",
              "gzip/ftgzip","sfnt/sfnt","truetype/truetype","smooth/smooth",
              "psnames/psnames","autofit/autofit","raster/raster"] {
        ft.file(format!("../oxft/ft/src/{f}.c"));
    }
    ft.compile("freetype");

    // --- main unit: wayland client + ffi + oxui + doomgeneric engine + platform ---
    let mut b = harness(&llvm_ar, &res_inc);
    b.include("../oxterm/include")
        .include("../oxwl/wl-include")
        .include("../oxffi/ffi-include")
        .include("../oxxkb/xkb/include")
        .include("../oxft/ft/include")
        .include("../oxui/include")
        .include("../oxterm/font")
        .include("../oxwl")
        .include(&dg) // doomgeneric.h, doomkeys.h, config.h, engine headers
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
    // doomgeneric engine (Makefile.soso list, minus its platform file).
    for f in ["am_map","d_event","d_items","d_iwad","d_loop","d_main","d_mode","d_net",
              "doomdef","doomgeneric","doomstat","dstrings","dummy","f_finale","f_wipe",
              "g_game","hu_lib","hu_stuff","i_cdmus","i_endoom","i_input","i_joystick",
              "i_scale","i_sound","i_system","i_timer","i_video","info","m_argv","m_bbox",
              "m_cheat","m_config","m_controls","m_fixed","m_menu","m_misc","m_random",
              "memio","p_ceilng","p_doors","p_enemy","p_floor","p_inter","p_lights",
              "p_map","p_maputl","p_mobj","p_plats","p_pspr","p_saveg","p_setup","p_sight",
              "p_spec","p_switch","p_telept","p_tick","p_user","r_bsp","r_data","r_draw",
              "r_main","r_plane","r_segs","r_sky","r_things","s_sound","sha1","sounds",
              "st_lib","st_stuff","statdump","tables","v_video","w_checksum","w_file",
              "w_file_stdc","w_main","w_wad","wi_stuff","z_zone"] {
        b.file(format!("{dg}/{f}.c"));
    }
    b.file("src/doomgeneric_oxbow.c");
    b.compile("doom");
}
