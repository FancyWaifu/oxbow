// Build vendored libxkbcommon (§48) for oxbow: the XKB keymap compiler + state
// machine, so the desktop decodes keycodes → keysyms → characters the standard
// way. We compile a keymap from a STRING only (the compositor ships a complete,
// self-contained keymap), so the RMLVO/file-loading paths are linked but never
// run. parser.c is bison-generated (committed alongside the source). Mirrors the
// libffi / libwayland harness.
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
        .include("xkb")            // config.h
        .include("xkb/include")    // xkbcommon/*.h
        .include("xkb/src")        // utils.h, keymap.h, ...
        .include("xkb/src/xkbcomp")
        .include("../../libc/include")
        .define("HAVE_CONFIG_H", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-everything")
        .opt_level(2);

    let srcs = [
        // core
        "xkb/src/atom.c",
        "xkb/src/context.c",
        "xkb/src/context-priv.c",
        "xkb/src/keymap.c",
        "xkb/src/keymap-priv.c",
        "xkb/src/keysym.c",
        "xkb/src/keysym-utf.c",
        "xkb/src/state.c",
        "xkb/src/text.c",
        "xkb/src/utf8.c",
        "xkb/src/util-list.c",
        "xkb/src/utils.c",
        // the keymap compiler (xkbcomp)
        "xkb/src/xkbcomp/action.c",
        "xkb/src/xkbcomp/ast-build.c",
        "xkb/src/xkbcomp/compat.c",
        "xkb/src/xkbcomp/expr.c",
        "xkb/src/xkbcomp/include.c",
        "xkb/src/xkbcomp/keycodes.c",
        "xkb/src/xkbcomp/keymap.c",
        "xkb/src/xkbcomp/keymap-dump.c",
        "xkb/src/xkbcomp/keywords.c",
        "xkb/src/xkbcomp/parser.c",
        "xkb/src/xkbcomp/rules.c",
        "xkb/src/xkbcomp/scanner.c",
        "xkb/src/xkbcomp/symbols.c",
        "xkb/src/xkbcomp/types.c",
        "xkb/src/xkbcomp/vmod.c",
        "xkb/src/xkbcomp/xkbcomp.c",
    ];
    for f in srcs {
        println!("cargo:rerun-if-changed={f}");
        b.file(f);
    }
    b.file("src/oxmain.c");
    b.compile("xkbcommon");
}
