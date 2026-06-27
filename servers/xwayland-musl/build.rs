// Build Xwayland (the xorg-server Wayland DDX) as a musl-personality oxbow program — a
// real X server that runs as a Wayland client of oxcomp (X apps beside Wayland apps).
// Software path only: no glamor/DRI/GBM/EGL (pixman renderer), ospoll POLL backend (no
// epoll). Compiled in cc groups (each needs its own includes/config): pixman, wayland+
// ffi, xkbcommon, libXau, libxcvt, the xserver core+DDX, glue (SHA1+stubs), and the
// musl-personality bridge. oxbow-rt (linked by the crate) provides _start + the __oxbow_*
// runtime shims. Sources are out-of-repo under ~/musl-oxbow (xserver/xorgproto/xtrans/
// pixman/libXau/libxcvt/libXfont2/drm/linux-headers); config headers + the 18 generated
// protocol headers were prepared per docs/linux-desktop-plan.md.
use std::process::Command;

fn meson_srcs(dir: &str) -> Vec<String> {
    // Extract the '<file>.c' entries the component's meson.build lists (matches the
    // manual build's source set exactly).
    let mb = format!("{dir}/meson.build");
    let out = Command::new("grep").args(["-oE", "'[a-zA-Z0-9_-]+\\.c'", &mb]).output();
    let mut v = vec![];
    if let Ok(o) = out {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            let f = l.trim_matches('\'');
            if !v.contains(&f.to_string()) {
                v.push(f.to_string());
            }
        }
    }
    v
}

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let home = std::env::var("HOME").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    let pers = format!("{dir}/../../userland/musl-personality");
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let mo = format!("{home}/musl-oxbow");
    let xs = format!("{mo}/xserver");
    let oxwl = format!("{dir}/../oxwl");
    let oxffi = format!("{dir}/../oxffi");
    let oxxkb = format!("{dir}/../oxxkb");
    assert!(std::path::Path::new(&xs).join("hw/xwayland/xwayland.c").exists(),
        "xserver not found at {xs} — clone xorg/xserver there (see docs/linux-desktop-plan.md)");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=glue.c");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={xs}/hw/xwayland");
    println!("cargo:rerun-if-changed={xs}/os");
    println!("cargo:rerun-if-changed={xs}/dix");
    println!("cargo:rerun-if-changed={xs}/xkb");
    println!("cargo:rerun-if-changed={xs}/include");
    println!("cargo:rerun-if-changed=us_keymap.c");

    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");

    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout).unwrap();
    let res_inc = format!("{}/include", res.trim());

    // cc-rs would otherwise use macOS `ar`, which can't archive ELF objects (→ empty
    // .a's). Use the rust-bundled llvm-ar, like havoc-musl does.
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);

    // xtrans is #included as <X11/Xtrans/Xtrans.h>; stage that layout in OUT_DIR.
    let xtinc = format!("{out}/xtinc");
    std::fs::create_dir_all(format!("{xtinc}/X11")).ok();
    let xt_link = format!("{xtinc}/X11/Xtrans");
    let _ = std::fs::remove_file(&xt_link);
    std::os::unix::fs::symlink(format!("{mo}/xtrans"), &xt_link).ok();

    let base = |b: &mut cc::Build| {
        b.archiver(&llvm_ar)
            .flag("-nostdinc").flag("-isystem").flag(&res_inc)
            .include(format!("{musl}/include")).include(format!("{musl}/obj/include"))
            .include(format!("{musl}/arch/x86_64")).include(format!("{musl}/arch/generic"))
            .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
            .flag("-Wno-everything").opt_level(2);
    };

    // 1. pixman (generic C; SIMD pass-through stubs come via the xserver glue group below)
    let mut px = cc::Build::new();
    base(&mut px);
    px.include(format!("{mo}/pixman/pixman")).define("HAVE_CONFIG_H", None);
    for f in ["pixman", "pixman-access", "pixman-access-accessors", "pixman-region32",
        "pixman-region16", "pixman-region64f", "pixman-image", "pixman-bits-image",
        "pixman-combine32", "pixman-combine-float", "pixman-general", "pixman-implementation",
        "pixman-noop", "pixman-utils", "pixman-matrix", "pixman-fast-path", "pixman-solid-fill",
        "pixman-trap", "pixman-edge", "pixman-edge-accessors", "pixman-gradient-walker",
        "pixman-linear-gradient", "pixman-radial-gradient", "pixman-conical-gradient",
        "pixman-glyph", "pixman-filter"] {
        px.file(format!("{mo}/pixman/pixman/{f}.c"));
    }
    px.compile("oxxpixman");

    // 2. wayland-client + libffi
    let mut wl = cc::Build::new();
    base(&mut wl);
    wl.include(format!("{oxwl}/wl-include")).include(format!("{oxffi}/ffi-include"))
        .include(&oxwl).define("HAVE_CONFIG_H", None)
        .define("_GNU_SOURCE", None).define("_POSIX_C_SOURCE", "200809L");
    for f in ["wayland-util", "connection", "wayland-os", "wayland-client", "wayland-protocol"] {
        wl.file(format!("{oxwl}/wl-src/{f}.c"));
    }
    for f in ["prep_cif", "types", "raw_api"] { wl.file(format!("{oxffi}/ffi-src/{f}.c")); }
    for f in ["ffi64", "ffiw64"] { wl.file(format!("{oxffi}/ffi-src/x86/{f}.c")); }
    for f in ["unix64.S", "win64.S"] { wl.file(format!("{oxffi}/ffi-src/x86/{f}")); }
    wl.include(format!("{oxffi}/ffi-src")).compile("oxxwl");

    // 3. xkbcommon
    let mut xk = cc::Build::new();
    base(&mut xk);
    xk.include(&oxxkb).include(format!("{oxxkb}/xkb")).include(format!("{oxxkb}/xkb/include"))
        .include(format!("{oxxkb}/xkb/src")).include(format!("{oxxkb}/xkb/src/xkbcomp"))
        .define("HAVE_CONFIG_H", None).define("HAVE_STRNDUP", "1").define("HAVE_ASPRINTF", "1")
        .define("HAVE_VASPRINTF", "1").define("HAVE_MMAP", "1");
    for f in ["atom", "context-priv", "context", "keymap-priv", "keymap", "keysym-utf",
        "keysym", "state", "text", "utf8", "util-list", "utils"] {
        xk.file(format!("{oxxkb}/xkb/src/{f}.c"));
    }
    for f in ["action", "ast-build", "compat", "expr", "include", "keycodes", "keymap-dump",
        "keymap", "keywords", "parser", "rules", "scanner", "symbols", "types", "vmod", "xkbcomp"] {
        xk.file(format!("{oxxkb}/xkb/src/xkbcomp/{f}.c"));
    }
    xk.compile("oxxxkb");

    // 4. libXau + libxcvt
    let mut au = cc::Build::new();
    base(&mut au);
    au.include(format!("{mo}/libXau/include")).include(format!("{mo}/xorgproto/include"));
    for f in ["AuDispose", "AuFileName", "AuGetAddr", "AuGetBest", "AuLock", "AuRead",
        "AuUnlock", "AuWrite"] { au.file(format!("{mo}/libXau/{f}.c")); }
    au.compile("oxxau");
    let mut cv = cc::Build::new();
    base(&mut cv);
    cv.include(format!("{mo}/libxcvt/include")).file(format!("{mo}/libxcvt/lib/libxcvt.c"));
    cv.compile("oxxcvt");

    // 4b. zlib (libXfont2 inflates the gzip'd builtin fonts)
    let mut zl = cc::Build::new();
    base(&mut zl);
    zl.include(format!("{mo}/zlib")).define("Z_HAVE_UNISTD_H", None);
    for f in ["adler32", "crc32", "inflate", "inftrees", "inffast", "zutil", "uncompr",
        "compress", "deflate", "trees", "gzlib", "gzread", "gzwrite", "gzclose", "infback"] {
        zl.file(format!("{mo}/zlib/{f}.c"));
    }
    zl.compile("oxxz");

    // 4c. libXfont2 — REAL server fonts via the compiled-in builtin font path (cursor/fixed
    //     are embedded gzip'd pcf in src/builtins/fonts.c). No font files, freetype, or
    //     fontenc needed; just bitmap/pcf + zlib. Replaces the glue.c xfont2_* stubs.
    let mut xf = cc::Build::new();
    base(&mut xf);
    let lxf = format!("{mo}/libXfont2");
    xf.define("__linux__", "1").define("HAVE_CONFIG_H", None)
        .include(&lxf).include(format!("{lxf}/include")).include(format!("{lxf}/src"))
        .include(format!("{lxf}/src/builtins")).include(format!("{mo}/xorgproto/include"))
        .include(format!("{mo}/zlib"));
    for f in ["fontaccel", "fontnames", "fontutil", "fontxlfd", "format", "miscutil",
        "patcache", "private", "reallocarray", "realpath", "strlcat", "strlcpy", "utilbitmap"] {
        xf.file(format!("{lxf}/src/util/{f}.c"));
    }
    for f in ["atom", "libxfontstubs"] { xf.file(format!("{lxf}/src/stubs/{f}.c")); }
    for f in ["bdfread", "bdfutils", "bitmap", "bitmapfunc", "bitmaputil", "bitscale",
        "fontink", "pcfread", "pcfwrite", "snfread"] {
        xf.file(format!("{lxf}/src/bitmap/{f}.c"));
    }
    for f in ["dir", "file", "fonts", "fpe", "render"] { xf.file(format!("{lxf}/src/builtins/{f}.c")); }
    for f in ["bitsource", "bufio", "catalogue", "decompress", "defaults", "dirfile",
        "fileio", "filewr", "fontdir", "fontfile", "fontscale", "gunzip", "register", "renderers"] {
        xf.file(format!("{lxf}/src/fontfile/{f}.c"));
    }
    xf.compile("oxxfont");

    // 5. THE xserver — core components + DDX + generated protocols. Excludes = the
    //    disabled-feature files (no Xinerama/security/xace/xselinux; our own SHA1).
    // Exactly the files that don't compile in this software/feature-reduced build
    // (Xinerama/screensaver/security/xace/xselinux off; our own SHA1 replaces xsha1).
    let exclude = ["xsha1", "panoramiX", "saver", "security", "xace",
        "xselinux_ext", "xselinux_hooks", "xselinux_label", "misyncshm"];
    let mut sv = cc::Build::new();
    base(&mut sv);
    sv.define("__linux__", "1").define("HAVE_DIX_CONFIG_H", None);
    for d in ["", "include", "os", "dix", "fb", "mi", "miext/damage", "miext/shadow",
        "miext/sync", "miext/cw", "render", "randr", "xfixes", "damageext", "composite",
        "Xext", "Xi", "xkb", "present", "hw/xwayland", "dbe", "record", "dri3", "gen-protocols"] {
        sv.include(if d.is_empty() { xs.clone() } else { format!("{xs}/{d}") });
    }
    for i in [format!("{mo}/xorgproto/include"), format!("{mo}/pixman/pixman"), xtinc.clone(),
        format!("{mo}/libXau/include"), format!("{mo}/libXfont2/include/X11/fonts"),
        format!("{mo}/libXfont2/include"), format!("{oxwl}/wl-include"),
        format!("{mo}/linux-headers"), format!("{mo}/drm"), format!("{mo}/drm/include/drm"),
        format!("{mo}/libxcvt/include"), format!("{mo}/libxkbfile/include"), format!("{mo}/libxkbfile")] {
        sv.include(i);
    }
    let comps = ["dix", "os", "mi", "fb", "render", "randr", "xfixes", "damageext",
        "composite", "dbe", "record", "Xext", "Xi", "xkb", "present", "miext/damage", "miext/sync"];
    for c in comps {
        for f in meson_srcs(&format!("{xs}/{c}")) {
            let stem = f.trim_end_matches(".c");
            if exclude.contains(&stem) { continue; }
            sv.file(format!("{xs}/{c}/{f}"));
        }
    }
    for f in ["xwayland", "xwayland-input", "xwayland-selection", "xwayland-cursor",
        "xwayland-pixmap", "xwayland-present", "xwayland-screen", "xwayland-shm",
        "xwayland-output", "xwayland-cvt", "xwayland-window", "xwayland-window-buffers",
        "xwayland-drm-lease", "xwayland-vidmode"] {
        sv.file(format!("{xs}/hw/xwayland/{f}.c"));
    }
    sv.file(format!("{xs}/mi/miinitext.c"));
    for e in std::fs::read_dir(format!("{xs}/gen-protocols")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) == Some("c") {
            sv.file(p);
        }
    }
    sv.compile("oxxserver");

    // 6. pixman SIMD pass-through stubs + 7. glue (SHA1, font/ffi/drm stubs) + 8. personality
    let mut gl = cc::Build::new();
    base(&mut gl);
    gl.define("_GNU_SOURCE", None);
    gl.file("glue.c");
    gl.file("us_keymap.c"); // compiled-in US keymap for the no-data-files XKB fallback
    gl.file(format!("{pers}/crt_glue.c")).file(format!("{pers}/oxbow_syscall.c")).include(&pers);
    // pixman arch get_implementations pass-throughs (we built generic-only)
    let stub = format!("{out}/pxsimd.c");
    std::fs::write(&stub, "typedef struct pixman_implementation_t pixman_implementation_t;\n\
        pixman_implementation_t *_pixman_arm_get_implementations(pixman_implementation_t *i){return i;}\n\
        pixman_implementation_t *_pixman_mips_get_implementations(pixman_implementation_t *i){return i;}\n\
        pixman_implementation_t *_pixman_ppc_get_implementations(pixman_implementation_t *i){return i;}\n\
        pixman_implementation_t *_pixman_riscv_get_implementations(pixman_implementation_t *i){return i;}\n\
        pixman_implementation_t *_pixman_x86_get_implementations(pixman_implementation_t *i){return i;}\n").unwrap();
    gl.file(&stub);
    gl.compile("oxxglue");
}
