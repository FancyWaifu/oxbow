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
    /// Next free physical address (frame-aligned).
    next: u64,
    /// One past the end of the region.
    end: u64,
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
        next: best_base,
        end: best_base + best_len,
    });

    (total, count)
}

/// Allocate one zeroed physical frame; `None` when the region is exhausted.
pub fn alloc_frame() -> Option<u64> {
    let mut guard = BUMP.lock();
    let bump = guard.as_mut()?;

    if bump.next + FRAME_SIZE > bump.end {
        return None;
    }
    let frame = bump.next;
    bump.next += FRAME_SIZE;
    drop(guard); // don't hold the lock across the zeroing write

    // Zero through the HHDM so the frame is safe to use as a page table.
    unsafe {
        core::ptr::write_bytes(crate::mm::phys_to_virt(frame) as *mut u8, 0, FRAME_SIZE as usize);
    }
    Some(frame)
}
