//! oxbow randomness — oxbow-libc getentropy (SYS_GETENTROPY), 256 bytes/call.
unsafe extern "C" {
    fn getentropy(buf: *mut u8, len: usize) -> i32;
}
pub fn fill_bytes(bytes: &mut [u8]) {
    for chunk in bytes.chunks_mut(256) {
        let r = unsafe { getentropy(chunk.as_mut_ptr(), chunk.len()) };
        assert!(r == 0, "oxbow getentropy failed");
    }
}
