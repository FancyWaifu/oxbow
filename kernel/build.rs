// Apply the kernel linker script to THIS crate only (rustc-link-arg affects
// only the kernel binary, so abi/ and rt/ still link as normal rlibs).
fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
}
