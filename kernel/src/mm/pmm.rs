//! Physical frame allocator — a one-way bump allocator.
//!
//! v0 never frees a frame (page tables, the one process's segments, and its
//! stack all live for the lifetime of the system), so a bump pointer over the
//! single largest usable region is all we need. Returned frames are zeroed
//! through the HHDM, so they're safe to use directly as page tables.
use limine::memory_map::EntryType;
use limine::response::MemoryMapResponse;
use spin::Mutex;

/// 4 KiB frame.
pub const FRAME_SIZE: u64 = 4096;

struct Bump {
    /// Start of the managed region (for stats()).
    base: u64,
    /// Next free physical address (frame-aligned).
    next: u64,
    /// One past the end of the region.
    end: u64,
    /// Head of the intrusive free list (0 = empty). A freed frame stores the
    /// physical address of the next free frame in its first 8 bytes, so the free
    /// list needs no side table and scales to all of RAM.
    free_head: u64,
}

static BUMP: Mutex<Option<Bump>> = Mutex::new(None);

/// Scan the Limine memory map, pick the largest usable region to bump-allocate
/// from, and return `(total usable bytes, usable region count)` for reporting.
pub fn init(memmap: &MemoryMapResponse) -> (u64, u32) {
    let mut total: u64 = 0;
    let mut count: u32 = 0;
    let mut best_base: u64 = 0;
    let mut best_len: u64 = 0;

    for entry in memmap.entries() {
        if entry.entry_type == EntryType::USABLE {
            total += entry.length;
            count += 1;
            if entry.length > best_len {
                best_len = entry.length;
                best_base = entry.base;
            }
        }
    }

    *BUMP.lock() = Some(Bump {
        base: best_base,
        next: best_base,
        end: best_base + best_len,
        free_head: 0,
    });

    (total, count)
}

/// `(used_bytes, total_bytes)` for the managed region — for a system monitor.
/// `used` is the bump high-water minus the frames returned to the free list.
pub fn stats() -> (u64, u64) {
    let guard = BUMP.lock();
    let Some(b) = guard.as_ref() else { return (0, 0) };
    let total = b.end - b.base;
    let mut freed = 0u64;
    let mut f = b.free_head;
    while f != 0 {
        freed += FRAME_SIZE;
        f = unsafe { *(crate::mm::phys_to_virt(f) as *const u64) };
    }
    let used = (b.next - b.base).saturating_sub(freed);
    (used, total)
}

/// Allocate one zeroed physical frame; `None` when memory is exhausted. Reuses a
/// freed frame (popped from the intrusive free list) before extending the bump.
pub fn alloc_frame() -> Option<u64> {
    let mut guard = BUMP.lock();
    let bump = guard.as_mut()?;

    let frame = if bump.free_head != 0 {
        let f = bump.free_head;
        // The next-free pointer lives in the frame's first 8 bytes.
        bump.free_head = unsafe { *(crate::mm::phys_to_virt(f) as *const u64) };
        f
    } else {
        if bump.next + FRAME_SIZE > bump.end {
            return None;
        }
        let f = bump.next;
        bump.next += FRAME_SIZE;
        f
    };
    drop(guard); // don't hold the lock across the zeroing write

    // Zero through the HHDM so the frame is safe to use as a page table.
    unsafe {
        core::ptr::write_bytes(crate::mm::phys_to_virt(frame) as *mut u8, 0, FRAME_SIZE as usize);
    }
    Some(frame)
}

/// Return a frame to the free list (push). The frame's first 8 bytes are
/// overwritten with the old list head — safe, the frame is no longer in use.
pub fn free_frame(frame: u64) {
    let mut guard = BUMP.lock();
    if let Some(bump) = guard.as_mut() {
        unsafe { *(crate::mm::phys_to_virt(frame) as *mut u64) = bump.free_head };
        bump.free_head = frame;
    }
}
