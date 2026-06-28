//! First-light musl program for oxbow. The runtime (`_start`, IPC shims) comes from
//! oxbow-rt[hosted]; `_start` calls `oxbow_main`, which the C crt bridge provides
//! (crt_glue.c) — it sets up a Linux initial stack and enters musl __libc_start_main.
//! All the C (bridge + dispatcher + the test main) + musl libc.a are linked by build.rs.
//!
//! Under the `hosted` feature oxbow-rt leaves the global allocator + panic handler to
//! its host (normally Rust std). A musl program has neither, and its Rust side is only
//! `_start` glue, so we satisfy the `no_std` requirement here: panic → exit, and the
//! (essentially unused) Rust allocator routes to musl's malloc.
#![no_std]
#![no_main]

extern crate oxbow_rt as _;

use core::alloc::{GlobalAlloc, Layout};

extern "C" {
    fn malloc(n: usize) -> *mut u8;
    fn free(p: *mut u8);
    fn __oxbow_exit(code: i32) -> !;
}

struct MuslAlloc;
unsafe impl GlobalAlloc for MuslAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // musl malloc is 16-byte aligned; the glue path needs no larger alignment.
        malloc(l.size())
    }
    unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
        free(p)
    }
}

#[global_allocator]
static ALLOC: MuslAlloc = MuslAlloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { __oxbow_exit(101) }
}
