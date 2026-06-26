// Build havoc (a real upstream Wayland terminal) as a musl-personality oxbow program.
// The first real third-party Wayland GUI app on oxbow. Compiles, in separate cc groups
// (each needs its OWN config.h / defines, so they can't share one include set):
//   1. libwayland-client + libffi  (from ../oxwl, ../oxffi) — config.h from oxwl
//   2. libxkbcommon                (from ../oxxkb)          — config.h from oxxkb
//   3. havoc itself + generated protocols + the wayland-cursor stub (out-of-repo at
//      ~/musl-oxbow/havoc, like the other musl apps)
//   4. the musl-personality bridge (crt_glue + oxbow_syscall)
// then links musl libc.a via musl-user.ld. Protocols are pre-generated with
// tools/wl-scanner.py (no Linux toolchain needed). Mirrors servers/darkhttpd-musl.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let home = std::env::var("HOME").unwrap();
    let pers = format!("{dir}/../../userland/musl-personality");
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let havoc = format!("{home}/musl-oxbow/havoc");
    let oxwl = format!("{dir}/../oxwl");
    let oxffi = format!("{dir}/../oxffi");
    let oxxkb = format!("{dir}/../oxxkb");
    assert!(std::path::Path::new(&havoc).join("main.c").exists(),
        "havoc not found at {havoc} — clone github.com/ii8/havoc there");

    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={havoc}");
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");

    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    ).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout,
    ).unwrap();
    let res_inc = format!("{}/include", res.trim());

    // Common musl/freestanding flags shared by every group.
    let base = |b: &mut cc::Build| {
        b.compiler("clang")
            .archiver(&llvm_ar)
            .flag("-nostdinc")
            .flag("-isystem").flag(&res_inc)
            .include(format!("{musl}/include"))
            .include(format!("{musl}/obj/include"))
            .include(format!("{musl}/arch/x86_64"))
            .include(format!("{musl}/arch/generic"))
            .flag("-ffreestanding")
            .flag("-fno-stack-protector")
            .flag("-fno-builtin")
            .flag("-Wno-everything")
            .define("_GNU_SOURCE", None)
            .define("_POSIX_C_SOURCE", "200809L")
            .opt_level(2);
    };

    // 1. libwayland-client + libffi (client side only; config.h from oxwl).
    let mut wl = cc::Build::new();
    base(&mut wl);
    wl.include(format!("{oxwl}/wl-include"))
        .include(format!("{oxffi}/ffi-include"))
        .include(&oxwl) // config.h
        .define("HAVE_CONFIG_H", None);
    for f in ["wl-src/wayland-util.c", "wl-src/connection.c", "wl-src/wayland-os.c",
              "wl-src/wayland-client.c", "wl-src/wayland-protocol.c"] {
        wl.file(format!("{oxwl}/{f}"));
    }
    for f in ["ffi-src/prep_cif.c", "ffi-src/types.c", "ffi-src/raw_api.c",
              "ffi-src/x86/ffi64.c", "ffi-src/x86/ffiw64.c",
              "ffi-src/x86/unix64.S", "ffi-src/x86/win64.S"] {
        wl.file(format!("{oxffi}/{f}"));
    }
    wl.compile("oxwl_musl");

    // 2. libxkbcommon (config.h from oxxkb; musl HAS strndup/asprintf/etc.).
    let mut xkb = cc::Build::new();
    base(&mut xkb);
    xkb.include(&oxxkb).include(format!("{oxxkb}/xkb")).include(format!("{oxxkb}/xkb/include"))
        .include(format!("{oxxkb}/xkb/src")).include(format!("{oxxkb}/xkb/src/xkbcomp"))
        .define("HAVE_CONFIG_H", None)
        .define("HAVE_STRNDUP", "1").define("HAVE_ASPRINTF", "1")
        .define("HAVE_VASPRINTF", "1").define("HAVE_MMAP", "1");
    for f in ["atom", "context", "context-priv", "keymap", "keymap-priv", "keysym",
              "keysym-utf", "state", "text", "utf8", "util-list", "utils",
              "xkbcomp/action", "xkbcomp/ast-build", "xkbcomp/compat", "xkbcomp/expr",
              "xkbcomp/include", "xkbcomp/keycodes", "xkbcomp/keymap", "xkbcomp/keymap-dump",
              "xkbcomp/keywords", "xkbcomp/parser", "xkbcomp/rules", "xkbcomp/scanner",
              "xkbcomp/symbols", "xkbcomp/types", "xkbcomp/vmod", "xkbcomp/xkbcomp"] {
        xkb.file(format!("{oxxkb}/xkb/src/{f}.c"));
    }
    xkb.compile("oxxkb_musl");

    // 3. havoc + generated protocols + cursor stub (no config.h; VERSION define).
    let mut app = cc::Build::new();
    base(&mut app);
    app.include(format!("{oxwl}/wl-include")).include(format!("{oxxkb}/xkb/include"))
        .include(&havoc)
        .define("VERSION", "\"0.7.0\"");
    for f in ["main", "glyph", "wayland-cursor", "xkb-compose-stub",
              "xdg-shell", "xdg-decoration-unstable-v1", "primary-selection-unstable-v1",
              "tsm/wcwidth", "tsm/shl-htable", "tsm/tsm-render", "tsm/tsm-screen",
              "tsm/tsm-selection", "tsm/tsm-unicode", "tsm/tsm-vte-charsets", "tsm/tsm-vte"] {
        app.file(format!("{havoc}/{f}.c"));
    }
    app.compile("havoc_app");

    // 4. the musl-personality bridge (Linux-syscall dispatcher + crt).
    let mut pb = cc::Build::new();
    base(&mut pb);
    pb.include(&pers);
    pb.file(format!("{pers}/crt_glue.c"));
    pb.file(format!("{pers}/oxbow_syscall.c"));
    pb.compile("oxbow_pers");
}
