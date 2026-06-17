//! sysmon — a tiny live system monitor, and the first net-new oxui app (§64).
//! The work is in sysmon.c (a draw callback); this is the no_std entry shim plus
//! a C-callable wrapper for the ambient meminfo syscall. (`ox_uptime_ms` is already
//! exported by libc.)
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
use oxbow_rt as rt;

/// `(used_kib, total_kib)` of the kernel's managed physical region.
#[no_mangle]
pub unsafe extern "C" fn ox_meminfo(used_kib: *mut u64, total_kib: *mut u64) {
    let (u, t) = rt::sys_meminfo();
    if !used_kib.is_null() {
        *used_kib = u;
    }
    if !total_kib.is_null() {
        *total_kib = t;
    }
}
