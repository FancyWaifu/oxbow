// Apply the USER link layout to this crate only (a low-half ET_EXEC at
// 0x200000), keeping it independent of the kernel's higher-half linker script.
fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
}
