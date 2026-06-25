// §96 Phase 4: shared build.rs helper to link an oxui consumer against /lib/liboxui.so
// dynamically (the recipe proven by sysmon in Phase 3). `include!` this from a consumer's
// build.rs and call `emit_oxui_dynlink(env!("CARGO_MANIFEST_DIR"))` INSTEAD of the static
// `-T user.ld`. The consumer must ALSO stop compiling oxui.c/oxui_text.c + wayland + ffi
// into its own unit (they live in liboxui.so now) and ship a dynamic `user-dyn.ld`
// (PHDRS phdr/interp/text/rodata/data/dynamic) next to its build.rs.
//
// What it does: builds liboxui.so first (so the linker can resolve the consumer's oxui_*
// calls + add DT_NEEDED liboxui.so), emits the dynamic link args (PT_INTERP=/lib/ld-oxbow,
// -z now, sysv hash), and AUTO-EXTRACTS the .so's undefined symbols (llvm-nm) to
// `--undefined` (force the static archives to provide them) + `--export-dynamic-symbol`
// (put them in the exe's .dynsym so ld-oxbow resolves the .so against the exe at runtime).
// Auto-extraction means the set never goes stale when oxui gains an import.
#[allow(dead_code)]
fn emit_oxui_dynlink(dir: &str) {
    use std::process::Command;
    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    )
    .unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_nm = format!("{}/lib/rustlib/{}/bin/llvm-nm", sysroot.trim(), host);

    let oxui_out = format!("{dir}/../oxui/out");
    let st = Command::new("bash").arg(format!("{dir}/../oxui/build-so.sh")).status().unwrap();
    assert!(st.success(), "build-so.sh failed");

    println!("cargo:rustc-link-arg=-T{dir}/user-dyn.ld");
    println!("cargo:rustc-link-arg=-dynamic-linker");
    println!("cargo:rustc-link-arg=/lib/ld-oxbow");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=now");
    println!("cargo:rustc-link-arg=--hash-style=sysv");
    println!("cargo:rustc-link-arg=-L{oxui_out}");
    println!("cargo:rustc-link-arg=-loxui");

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
    println!("cargo:rerun-if-changed={dir}/user-dyn.ld");
    println!("cargo:rerun-if-changed={dir}/../oxui/oxui.c");
    println!("cargo:rerun-if-changed={dir}/../oxui/oxui_text.c");
}
