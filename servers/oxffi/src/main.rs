//! ffi-test — prove the vendored libffi (x86_64 SysV) ffi_call path works on
//! oxbow. The real work is in oxmain.c; this is the no_std entry shim.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
