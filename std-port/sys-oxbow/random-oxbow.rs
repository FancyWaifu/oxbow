//! oxbow randomness — oxbow-rt's `__oxbow_getentropy` shim (SYS_GETENTROPY).
unsafe extern "C" {
    fn __oxbow_getentropy(buf: *mut u8, len: usize) -> i32;
}
pub fn fill_bytes(bytes: &mut [u8]) {
    for chunk in bytes.chunks_mut(256) {
        let r = unsafe { __oxbow_getentropy(chunk.as_mut_ptr(), chunk.len()) };
        assert!(r == 0, "oxbow getentropy failed");
    }
}
