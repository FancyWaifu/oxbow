//! oxbow errno + ErrorKind mapping. oxbow-libc uses Linux-ish errno values and a
//! global `errno` symbol.
use crate::io::ErrorKind;

unsafe extern "C" {
    #[link_name = "errno"]
    static mut C_ERRNO: i32;
}

pub fn errno() -> i32 {
    unsafe { (&raw const C_ERRNO).read() }
}
pub fn set_errno(e: i32) {
    unsafe { (&raw mut C_ERRNO).write(e) };
}
pub fn is_interrupted(code: i32) -> bool {
    code == 4 // EINTR
}
pub fn error_string(code: i32) -> String {
    format!("oxbow os error {code}")
}
pub fn decode_error_kind(code: i32) -> ErrorKind {
    use ErrorKind::*;
    match code {
        1 => PermissionDenied,    // EPERM
        2 => NotFound,            // ENOENT
        11 => WouldBlock,         // EAGAIN
        13 => PermissionDenied,   // EACCES
        17 => AlreadyExists,      // EEXIST
        21 => IsADirectory,       // EISDIR
        22 => InvalidInput,       // EINVAL
        32 => BrokenPipe,         // EPIPE
        110 => TimedOut,          // ETIMEDOUT
        111 => ConnectionRefused, // ECONNREFUSED
        _ => Uncategorized,
    }
}
