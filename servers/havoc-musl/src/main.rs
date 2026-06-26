//! havoc — a real upstream Wayland terminal emulator (github.com/ii8/havoc), built
//! as a musl-personality oxbow program. The FIRST real third-party Wayland GUI app on
//! oxbow: it speaks the real wire protocol to oxcomp via libwayland (built for musl),
//! renders with its own stb_truetype rasterizer, and drives the terminal with bundled
//! libtsm. The Rust side is only `_start` glue (oxbow-rt[hosted]); all the C (havoc +
//! libwayland + xkbcommon + the personality bridge) + musl libc.a are linked by build.rs.
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
