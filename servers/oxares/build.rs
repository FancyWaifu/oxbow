// Build vendored c-ares (the leading-standard async DNS stub resolver, MIT) for
// oxbow, minus the OS-config-file and event-backend sources (oxbow has none of
// epoll/kqueue/poll/select/win32, and we set servers programmatically + drive
// ares_process manually). Mirrors the lwext4/BearSSL C-port harness.
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");

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
        .include("config") // ares_config.h + ares_build.h
        .include("cares-include") // public ares.h
        .include("cares-src/lib") // internal headers
        .include("cares-src/lib/include")
        .include("../../libc/include")
        .define("HAVE_CONFIG_H", None)
        .define("CARES_STATICLIB", None)
        .define("CARES_BUILDING_LIBRARY", None)
        .flag("-ffreestanding")
        .flag("-fno-stack-protector")
        .flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration")
        .flag("-Wno-everything")
        .opt_level(2);

    // Platform / event-backend sources oxbow doesn't have.
    let skip = [
        "ares_android.c",
        "ares_sysconfig_mac.c",
        "ares_sysconfig_win.c",
        "ares_sysconfig_files.c",
        "ares_event_epoll.c",
        "ares_event_kqueue.c",
        "ares_event_poll.c",
        "ares_event_select.c",
        "ares_event_win32.c",
        "ares_event_configchg.c",
        "ares_event_wake_pipe.c",
        "ares_event_thread.c",
    ];
    fn walk(dir: &str, skip: &[&str], b: &mut cc::Build) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(p.to_str().unwrap(), skip, b);
            } else if p.extension().map(|x| x == "c").unwrap_or(false) {
                let name = p.file_name().unwrap().to_str().unwrap();
                if !skip.contains(&name) {
                    println!("cargo:rerun-if-changed={}", p.display());
                    b.file(&p);
                }
            }
        }
    }
    walk("cares-src/lib", &skip, &mut b);
    b.file("src/oxmain.c");
    b.compile("cares");
}
