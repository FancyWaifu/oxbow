//! oxbow System allocator — delegates to oxbow-rt's hosted slab shims
//! (`__oxbow_alloc`/`_zeroed`/`_dealloc`). realloc uses std's generic fallback.
use super::realloc_fallback;
use crate::alloc::{GlobalAlloc, Layout, System};

unsafe extern "C" {
    fn __oxbow_alloc(size: usize, align: usize) -> *mut u8;
    fn __oxbow_alloc_zeroed(size: usize, align: usize) -> *mut u8;
    fn __oxbow_dealloc(ptr: *mut u8, size: usize, align: usize);
}

unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { __oxbow_alloc(layout.size(), layout.align()) }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { __oxbow_alloc_zeroed(layout.size(), layout.align()) }
    }
    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { __oxbow_dealloc(ptr, layout.size(), layout.align()) }
    }
    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { realloc_fallback(self, ptr, layout, new_size) }
    }
}
