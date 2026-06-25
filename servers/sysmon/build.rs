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

    // §96 Phase 3: sysmon links oxui DYNAMICALLY from /lib/liboxui.so instead of
    // baking oxui.c + oxui_text.c into its own binary. Build the .so first so the
    // linker can resolve sysmon.c's oxui_* calls against it (DT_NEEDED liboxui.so).
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    )
    .unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let llvm_nm = format!("{}/lib/rustlib/{}/bin/llvm-nm", sysroot.trim(), host);

    // §96 Phase 3: sysmon links oxui DYNAMICALLY from /lib/liboxui.so instead of
    // baking oxui.c + oxui_text.c into its own binary. Build the .so first so the
    // linker can resolve sysmon.c's oxui_* calls against it (DT_NEEDED liboxui.so).
    let oxui_out = format!("{dir}/../oxui/out");
    let st = Command::new("bash").arg(format!("{dir}/../oxui/build-so.sh")).status().unwrap();
    assert!(st.success(), "build-so.sh failed");

    // Dynamic link: PT_INTERP=/lib/ld-oxbow, eager binding, sysv hash (ld-oxbow reads
    // DT_HASH nchain).
    println!("cargo:rustc-link-arg=-T{dir}/user-dyn.ld");
    println!("cargo:rustc-link-arg=-dynamic-linker");
    println!("cargo:rustc-link-arg=/lib/ld-oxbow");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=now");
    println!("cargo:rustc-link-arg=--hash-style=sysv");
    println!("cargo:rustc-link-arg=-L{oxui_out}");
    println!("cargo:rustc-link-arg=-loxui");

    // sysmon.c doesn't directly reference the libc/wayland/ffi/xkb/freetype symbols
    // that liboxui.so imports (oxui did, and it's now in the .so) — so the linker won't
    // pull those archive members on its own (a shared lib's UNDEFs are runtime, not
    // link-time), and they wouldn't be in sysmon's .dynsym for ld-oxbow to resolve the
    // .so against. Two link args, both driven by the .so's undefined-symbol set (auto-
    // extracted with llvm-nm so it never goes stale when oxui adds an import):
    //   --undefined=SYM    force the archive member defining SYM to be pulled in.
    //   --dynamic-list F   export EXACTLY these symbols into .dynsym. (NOT --export-
    //     dynamic: that exports/retains ALL globals, which drags in unreferenced
    //     server-side wayland code — wl_display_connect->unsetenv, wl_os_accept_cloexec
    //     ->accept — that oxbow-libc doesn't have. --dynamic-list lets --gc-sections
    //     still drop those, while exporting the symbols the .so actually needs.)
    let nm = Command::new(&llvm_nm)
        .args(["--undefined-only", "--no-sort", &format!("{oxui_out}/liboxui.so")])
        .output()
        .unwrap();
    let mut retained = 0;
    for line in String::from_utf8_lossy(&nm.stdout).lines() {
        if let Some(sym) = line.split_whitespace().last() {
            if !sym.is_empty() && sym != "U" {
                println!("cargo:rustc-link-arg=--undefined={sym}");
                println!("cargo:rustc-link-arg=--export-dynamic-symbol={sym}");
                retained += 1;
            }
        }
    }
    assert!(retained > 0, "no undefined symbols extracted from liboxui.so");
    println!("cargo:rerun-if-changed=user-dyn.ld");
    println!("cargo:rerun-if-changed=src/sysmon.c");
    println!("cargo:rerun-if-changed=../oxui/oxui.c");
    println!("cargo:rerun-if-changed=../oxui/oxui_text.c");
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
