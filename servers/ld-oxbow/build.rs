// §96: ld-oxbow links as a static ET_EXEC HIGH in the user low-half (0x1000_0000)
// so it never overlaps the dynamic executable it links (at 0x200000) or the shared
// objects it maps (bumped from 0x3000_0000). Entry is oxbow-rt's _start.
fn main() {
    let dir = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
}
