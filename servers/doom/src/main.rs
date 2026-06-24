//! DOOM (1993) on oxbow, via the doomgeneric port rendering into an oxui window.
//! All the work is in C (doomgeneric_oxbow.c + the doomgeneric engine, linked against
//! oxbow-libc + the Wayland/oxui client stack by build.rs); this is just the no_std
//! entry shim plus `ox_dbg`, a direct console writer the platform uses for progress
//! lines (its libc stdout isn't a tty endpoint, so printf can't reach the console).
//! `ox_uptime_ms` (used for DG_GetTicksMs) is already exported by oxbow-libc.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
use oxbow_rt as rt;

/// Write a debug line straight to the BOOT_CONSOLE cap the compositor handed DOOM on
/// slot 2 (= the kernel serial console). Unlike libc stdout (a tty/pipe endpoint), the
/// console cap accepts console_write, so this is the reliable progress channel.
#[no_mangle]
pub unsafe extern "C" fn ox_dbg(p: *const u8, len: usize) {
    let _ = rt::sys_console_write(oxbow_abi::BOOT_CONSOLE, p, len);
}
