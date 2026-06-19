//! oxbow program arguments — split the SPAWN_ARGV string (the boot-module cmdline
//! or the shell's argv), via oxbow-rt's `__oxbow_argv` shim.
pub use super::common::Args;
use crate::ffi::OsString;

unsafe extern "C" {
    fn __oxbow_argv(len: *mut usize) -> *const u8;
}

pub fn args() -> Args {
    let mut len = 0usize;
    let ptr = unsafe { __oxbow_argv(&mut len) };
    if ptr.is_null() || len == 0 {
        return Args::new(crate::vec::Vec::new());
    }
    let bytes = unsafe { crate::slice::from_raw_parts(ptr, len) };
    let v: crate::vec::Vec<OsString> = bytes
        .split(|&b| b == b' ')
        .filter(|s| !s.is_empty())
        .map(|s| OsString::from(crate::string::String::from_utf8_lossy(s).into_owned()))
        .collect();
    Args::new(v)
}
