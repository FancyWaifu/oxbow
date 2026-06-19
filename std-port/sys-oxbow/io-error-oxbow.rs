//! oxbow errno + ErrorKind mapping. No external libc, so std owns the errno cell;
//! oxbow uses Linux-ish errno values.
use crate::io::ErrorKind;

static mut ERRNO: i32 = 0;

pub fn errno() -> i32 {
    unsafe { (&raw const ERRNO).read() }
}
pub fn set_errno(e: i32) {
    unsafe { (&raw mut ERRNO).write(e) };
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
        1 => PermissionDenied,
        2 => NotFound,
        11 => WouldBlock,
        13 => PermissionDenied,
        17 => AlreadyExists,
        21 => IsADirectory,
        22 => InvalidInput,
        32 => BrokenPipe,
        110 => TimedOut,
        111 => ConnectionRefused,
        _ => Uncategorized,
    }
}
