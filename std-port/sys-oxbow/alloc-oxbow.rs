//! oxbow System allocator — routes to oxbow-libc malloc/free/realloc/calloc.
//! oxbow-libc has no aligned_alloc, so over-aligned requests over-allocate and
//! stash the real base just before the returned pointer.
use super::{MIN_ALIGN, realloc_fallback};
use crate::alloc::{GlobalAlloc, Layout, System};
use crate::ptr;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn calloc(nmemb: usize, size: usize) -> *mut u8;
    fn free(ptr: *mut u8);
    fn realloc(ptr: *mut u8, size: usize) -> *mut u8;
}

unsafe fn aligned_malloc(layout: &Layout) -> *mut u8 {
    let align = layout.align();
    let hdr = core::mem::size_of::<usize>();
    let total = layout.size() + align + hdr;
    let base = unsafe { malloc(total) };
    if base.is_null() {
        return ptr::null_mut();
    }
    let aligned = ((base as usize) + hdr + align - 1) & !(align - 1);
    unsafe { ((aligned - hdr) as *mut usize).write(base as usize) };
    aligned as *mut u8
}

#[inline]
fn small(layout: &Layout, n: usize) -> bool {
    layout.align() <= MIN_ALIGN && layout.align() <= n
}

unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if small(&layout, layout.size()) { unsafe { malloc(layout.size()) } } else { unsafe { aligned_malloc(&layout) } }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if small(&layout, layout.size()) {
            unsafe { calloc(layout.size(), 1) }
        } else {
            let p = unsafe { self.alloc(layout) };
            if !p.is_null() { unsafe { ptr::write_bytes(p, 0, layout.size()) }; }
            p
        }
    }
    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if small(&layout, layout.size()) {
            unsafe { free(ptr) }
        } else {
            let base = unsafe { ptr.cast::<usize>().sub(1).read() };
            unsafe { free(base as *mut u8) }
        }
    }
    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if small(&layout, new_size) { unsafe { realloc(ptr, new_size) } } else { unsafe { realloc_fallback(self, ptr, layout, new_size) } }
    }
}
