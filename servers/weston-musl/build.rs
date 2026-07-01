// Build the real upstream Weston compositor (libweston 9.0) as a musl-personality oxbow
// program — software path only (pixman renderer, no EGL/GL/DRM/libinput/dbus). Compiled in
// cc groups against the oxbow stack: pixman, libwayland(server+client)+ffi, xkbcommon,
// libweston core (+ generated protocols), and the personality bridge + glue. oxbow-rt[hosted]
// provides _start; the C `main` (glue.c stub in P1) is the entry. Sources out-of-repo under
// ~/musl-oxbow (weston-9.0.0, pixman, wayland-protocols, drm, linux-headers). Deps in-repo:
// oxwl (libwayland), oxffi (libffi), oxxkb (xkbcommon). See docs/weston-port.md.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let home = std::env::var("HOME").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    let pers = format!("{dir}/../../userland/musl-personality");
    let musl = format!("{home}/musl-oxbow/musl-1.2.5");
    let mo = format!("{home}/musl-oxbow");
    let w = format!("{mo}/weston-9.0.0");
    let wp = format!("{mo}/wayland-protocols");
    let oxwl = format!("{dir}/../oxwl");
    let oxffi = format!("{dir}/../oxffi");
    let oxxkb = format!("{dir}/../oxxkb");
    let scanner = format!("{dir}/../../tools/wl-scanner.py");
    assert!(std::path::Path::new(&w).join("libweston/compositor.c").exists(),
        "weston not found at {w} — clone weston tag 9.0.0 there (see docs/weston-port.md)");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=glue.c");
    println!("cargo:rerun-if-changed=oxbow-backend.c");
    println!("cargo:rerun-if-changed=oxbow-main.c");
    println!("cargo:rerun-if-changed=config.h");
    println!("cargo:rerun-if-changed=git-version.h");
    println!("cargo:rerun-if-changed={pers}");
    println!("cargo:rerun-if-changed={w}/libweston");
    println!("cargo:rerun-if-changed={scanner}");

    println!("cargo:rustc-link-arg=-T{pers}/musl-user.ld");
    println!("cargo:rustc-link-arg=--start-group");
    println!("cargo:rustc-link-arg={musl}/lib/libc.a");
    println!("cargo:rustc-link-arg=--end-group");

    let res_inc = format!("{}/include", String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout).unwrap().trim());
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);

    // --- generate libweston/version.h from version.h.in (fill 9.0.0) ---
    let vin = std::fs::read_to_string(format!("{w}/include/libweston/version.h.in")).unwrap();
    let vh = vin.replace("@WESTON_VERSION_MAJOR@", "9").replace("@WESTON_VERSION_MINOR@", "0")
        .replace("@WESTON_VERSION_MICRO@", "0").replace("@WESTON_VERSION@", "9.0.0");
    std::fs::create_dir_all(format!("{out}/libweston")).unwrap();
    std::fs::write(format!("{out}/libweston/version.h"), vh).unwrap();

    // --- generate the protocol server-headers + interface-code the core links ---
    let gen = format!("{out}/gen");
    std::fs::create_dir_all(&gen).unwrap();
    let protocols = [
        "linux-dmabuf-unstable-v1", "linux-explicit-synchronization-unstable-v1",
        "input-method-unstable-v1", "input-timestamps-unstable-v1", "presentation-time",
        "pointer-constraints-unstable-v1", "relative-pointer-unstable-v1", "text-cursor-position",
        "text-input-unstable-v1", "viewporter", "xdg-output-unstable-v1", "weston-screenshooter",
        "weston-touch-calibration", "weston-content-protection", "weston-debug",
        "weston-direct-display",
        // libweston-desktop (xdg-shell) — P4/P5
        "xdg-shell", "xdg-shell-unstable-v6",
    ];
    let find_xml = |name: &str| -> String {
        let o = Command::new("find").args([&wp, &format!("{w}/protocol"), "-name",
            &format!("{name}.xml")]).output().unwrap();
        String::from_utf8(o.stdout).unwrap().lines().next()
            .unwrap_or_else(|| panic!("no XML for protocol {name}")).to_string()
    };
    let mut proto_c = vec![];
    for name in protocols {
        let xml = find_xml(name);
        for (mode, ext) in [("server-header", "server-protocol.h"), ("private-code", "protocol.c")] {
            let o = Command::new("python3").args([&scanner, mode, &xml]).output().unwrap();
            assert!(o.status.success(), "scanner failed on {name}: {}",
                String::from_utf8_lossy(&o.stderr));
            std::fs::write(format!("{gen}/{name}-{ext}"), o.stdout).unwrap();
        }
        proto_c.push(format!("{gen}/{name}-protocol.c"));
    }

    let base = |b: &mut cc::Build| {
        b.archiver(&llvm_ar)
            .flag("-nostdinc").flag("-isystem").flag(&res_inc)
            .include(format!("{musl}/include")).include(format!("{musl}/obj/include"))
            .include(format!("{musl}/arch/x86_64")).include(format!("{musl}/arch/generic"))
            .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
            .flag("-Wno-everything").opt_level(2);
    };

    // 1. pixman (generic C; per-arch SIMD dispatchers stubbed in glue.c)
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
    px.compile("oxwpixman");

    // 2. libwayland (server + client + wire) + libffi
    let mut wl = cc::Build::new();
    base(&mut wl);
    wl.include(format!("{oxwl}/wl-include")).include(format!("{oxffi}/ffi-include"))
        .include(&oxwl).include(format!("{oxffi}/ffi-src"))
        .define("HAVE_CONFIG_H", None).define("_GNU_SOURCE", None)
        .define("_POSIX_C_SOURCE", "200809L");
    for f in ["wayland-util", "connection", "wayland-os", "wayland-protocol",
        "xdg-shell-protocol", "event-loop", "wayland-server", "wayland-client", "wayland-shm"] {
        wl.file(format!("{oxwl}/wl-src/{f}.c"));
    }
    for f in ["prep_cif", "types", "raw_api"] { wl.file(format!("{oxffi}/ffi-src/{f}.c")); }
    for f in ["ffi64", "ffiw64"] { wl.file(format!("{oxffi}/ffi-src/x86/{f}.c")); }
    for f in ["unix64.S", "win64.S"] { wl.file(format!("{oxffi}/ffi-src/x86/{f}")); }
    wl.compile("oxwwl");

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
    xk.compile("oxwxkb");

    // 4. libweston core (srcs_libweston) + shared math/os + generated protocol code
    let mut we = cc::Build::new();
    base(&mut we);
    we.define("__linux__", "1").define("HAVE_CONFIG_H", None).define("_GNU_SOURCE", None);
    // config.h + git-version.h live in the crate dir; version.h under OUT_DIR/libweston;
    // generated protocol headers under OUT_DIR/gen; libweston public headers in include/.
    for i in [dir.to_string(), format!("{dir}/shim"), out.clone(), gen.clone(), w.clone(),
        format!("{w}/include"), format!("{w}/libweston"), format!("{w}/shared"),
        format!("{w}/libweston-desktop"),
        format!("{oxwl}/wl-include"), format!("{mo}/pixman/pixman"),
        format!("{oxxkb}/xkb/include"), format!("{mo}/drm/include/drm"),
        format!("{mo}/linux-headers")] {
        we.include(i);
    }
    for f in ["animation", "bindings", "clipboard", "compositor", "content-protection",
        "data-device", "input", "linux-dmabuf", "linux-explicit-synchronization",
        "linux-sync-file", "log", "noop-renderer", "pixel-formats", "pixman-renderer",
        "plugin-registry", "screenshooter", "timeline", "touch-calibration", "weston-log-wayland",
        "weston-log-file", "weston-log-flight-rec", "weston-log", "weston-direct-display", "zoom"] {
        we.file(format!("{w}/libweston/{f}.c"));
    }
    for f in ["matrix", "os-compatibility", "xalloc"] {
        we.file(format!("{w}/shared/{f}.c"));
    }
    for c in &proto_c { we.file(c); }
    // libweston-desktop (xdg-shell / wl-shell impl); skip xwayland.c (needs xwayland)
    for f in ["libweston-desktop", "client", "seat", "surface", "wl-shell",
        "xdg-shell", "xdg-shell-v6"] {
        we.file(format!("{w}/libweston-desktop/{f}.c"));
    }
    // the oxbow backend (pixman -> FB_MMIO) + input (seat) + shell + the frontend main
    we.file("oxbow-backend.c");
    we.file("oxbow-input.c");
    we.file("oxbow-shell.c");
    we.file("oxbow-main.c");
    we.compile("oxweston");

    // 5. personality bridge + glue (stub main + pixman SIMD pass-throughs)
    let mut gl = cc::Build::new();
    base(&mut gl);
    gl.define("_GNU_SOURCE", None);
    gl.file("glue.c");
    gl.file(format!("{pers}/crt_glue.c")).file(format!("{pers}/oxbow_syscall.c")).include(&pers);
    gl.compile("oxwglue");
}
