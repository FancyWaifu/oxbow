use std::fs;
use std::process::Command;

fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
    println!("cargo:rerun-if-changed=config/curl_config.h");
    println!("cargo:rerun-if-changed=src/oxmain.c");

    let sysroot = String::from_utf8(
        Command::new("rustc").args(["--print", "sysroot"]).output().unwrap().stdout,
    ).unwrap();
    let host = std::env::var("HOST").unwrap();
    let llvm_ar = format!("{}/lib/rustlib/{}/bin/llvm-ar", sysroot.trim(), host);
    let res = String::from_utf8(
        Command::new("clang").args(["-print-resource-dir"]).output().unwrap().stdout,
    ).unwrap();
    let res_inc = format!("{}/include", res.trim());

    let mut b = cc::Build::new();
    b.compiler(std::env::var("CC").unwrap_or_else(|_| "clang".into()))
        .archiver(&llvm_ar)
        .flag("-nostdinc").flag("-isystem").flag(&res_inc)
        .include("config")           // curl_config.h
        .include("curl/include")     // public curl/*.h
        .include("curl/lib")         // internal headers
        .include("../../libc/include")
        .define("HAVE_CONFIG_H", None)
        .define("BUILDING_LIBCURL", None)
        .define("__oxbow__", None)
        .flag("-ffreestanding").flag("-fno-stack-protector").flag("-fno-builtin")
        .flag("-Wno-implicit-function-declaration").flag("-Wno-everything")
        .opt_level(2);
    // libcurl core + vtls (no-backend stubs) + vauth. Skip vquic/vssh (HTTP/3/SSH).
    for d in ["curl/lib", "curl/lib/vtls", "curl/lib/vauth"] {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_str().unwrap().to_string();
            let skip = ["http_aws_sigv4.c"];
            if p.extension().map(|x| x == "c").unwrap_or(false) && !skip.contains(&name.as_str()) {
                println!("cargo:rerun-if-changed={}", p.display());
                b.file(&p);
            }
        }
    }
    b.file("src/oxmain.c");
    b.compile("curl");
}
