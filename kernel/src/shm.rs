//! shm — shared multi-page memory regions (§41), the backing for POSIX
//! memfd/mmap and thus Wayland's wl_shm pixel buffers. A region owns N physical
//! frames; mapping it lands those frames at N consecutive page-aligned vaddrs in
//! the caller's address space. The region is a capability (ObjectRef::Shm), so a
//! client can pass it to a compositor over a channel (SCM_RIGHTS) and BOTH map
//! the same frames — genuine shared memory between two processes.
//!
//! Frames need not be physically contiguous; the page tables give a contiguous
//! virtual view. Regions are reset in place (never built by value — a Region is
//! ~4 KiB and would overflow the kernel stack, like the channel pool).
use crate::sync::DiagMutex;

// Each double-buffered Wayland client needs 2 shm regions; the desktop has 3
// clients (terminal, rings, sysmon) = 6, and 4 was exactly enough for 2 clients —
// so adding the 3rd window left it unable to allocate a buffer and it never mapped.
// 16 covers 8 double-buffered windows with headroom. (Each region is ~4 KiB of
// static frame-table, so 16 is 64 KiB.)
const NREGIONS: usize = 16;
/// Max pages per region: 4096 * 4 KiB = 16 MiB. MUST cover a FULL-SCREEN client
/// wl_shm buffer at the real display resolution, because a window maximizes to the
/// whole screen and re-renders NATIVELY at that size (text stays the same size, the
/// window just gains room). 1920x1080x4 = 2025 pages, 2560x1440x4 = 3600 pages — so
/// 1024 (4 MiB, only a 1280x800 buffer) made every maximize on a 1080p display fail
/// to allocate, fall back to the small buffer, and get UPSCALED (blurry + slow). The
/// gpu/oxcomp scanout buffers live here too. (16 regions x 16 MiB frame-table = 512
/// KiB static; frames are only allocated on demand by `create`.)
const MAX_PAGES: usize = 4096;

#[derive(Clone, Copy)]
struct Region {
    in_use: bool,
    /// Handle-table references to this region across ALL processes (§41 refcount).
    /// `create` starts at 0; each installed handle (`alloc_slot`/`install`) increfs,
    /// each dropped handle (`close`/`close_all`) decrefs. Freed at 0 — so a wl_shm
    /// buffer shared with the compositor (grant-by-copy = 2 handles) is reclaimed only
    /// once BOTH Xwayland and oxcomp drop it. Without this, every X pixmap leaked a
    /// region and the 16-slot pool exhausted → Xwayland died.
    rc: u32,
    /// The Memory budget that paid for the frames, refunded when the region frees.
    mem_idx: u8,
    npages: usize,
    frames: [u64; MAX_PAGES],
}
impl Region {
    const fn new() -> Self {
        Region { in_use: false, rc: 0, mem_idx: 0, npages: 0, frames: [0; MAX_PAGES] }
    }
}

static REGIONS: DiagMutex<[Region; NREGIONS]> = DiagMutex::new("REGIONS", [Region::new(); NREGIONS]);

/// Allocate a region of `npages` frames. Returns its pool index, or None if the
/// pool is full, `npages` is out of range, or the PMM is exhausted (any frames
/// grabbed on a partial failure are returned).
pub fn create(npages: usize) -> Option<u8> {
    if npages == 0 || npages > MAX_PAGES {
        return None;
    }
    let mut regs = REGIONS.lock();
    let idx = regs.iter().position(|r| !r.in_use)?;
    let r = &mut regs[idx];
    // Allocate frames; on shortfall, free what we took and fail.
    for i in 0..npages {
        match crate::mm::pmm::alloc_frame() {
            Some(phys) => r.frames[i] = phys,
            None => {
                for &p in &r.frames[..i] {
                    crate::mm::pmm::free_frame(p);
                }
                return None;
            }
        }
    }
    r.in_use = true;
    r.rc = 0; // the first installed handle increfs to 1
    r.mem_idx = 0;
    r.npages = npages;
    Some(idx as u8)
}

/// Allocate a region of `npages` PHYSICALLY CONTIGUOUS frames (§90). Unlike
/// `create`, the frames form one contiguous run (carved off the bump pointer), so
/// `phys_base` names the whole region with a single address — exactly what a GPU
/// scanout backing needs for RESOURCE_ATTACH_BACKING with one mem-entry. Returns
/// the pool index, or None if the pool is full / out of range / PMM exhausted.
pub fn create_contig(npages: usize) -> Option<u8> {
    if npages == 0 || npages > MAX_PAGES {
        return None;
    }
    let mut regs = REGIONS.lock();
    let idx = regs.iter().position(|r| !r.in_use)?;
    let base = crate::mm::pmm::alloc_contig(npages as u64)?;
    let r = &mut regs[idx];
    for i in 0..npages {
        r.frames[i] = base + (i as u64) * 4096;
    }
    r.in_use = true;
    r.rc = 0; // the first installed handle increfs to 1
    r.mem_idx = 0;
    r.npages = npages;
    Some(idx as u8)
}

/// Physical base address of region `idx`'s first frame. Meaningful as the whole
/// region's base only for a `create_contig` region; for a scattered region it's
/// just the first page. 0 if the region is free.
pub fn phys_base(idx: u8) -> u64 {
    let regs = REGIONS.lock();
    let r = &regs[idx as usize];
    if r.in_use {
        r.frames[0]
    } else {
        0
    }
}

/// Total byte size of region `idx` (npages * 4096), or 0 if free.
pub fn size(idx: u8) -> usize {
    let regs = REGIONS.lock();
    let r = &regs[idx as usize];
    if r.in_use {
        r.npages * 4096
    } else {
        0
    }
}

/// Map region `idx` into the live address space `pml4` at `vaddr` (writable),
/// one page per frame. Returns the page count mapped, or 0 on failure.
pub fn map(idx: u8, pml4: u64, vaddr: u64, writable: bool) -> usize {
    let regs = REGIONS.lock();
    let r = &regs[idx as usize];
    if !r.in_use {
        return 0;
    }
    // Pre-check the whole range is unmapped before touching anything.
    if crate::mm::vm::probe_user_range(pml4, vaddr, r.npages as u64).is_err() {
        return 0;
    }
    for i in 0..r.npages {
        crate::mm::vm::map_user_4k_live(pml4, vaddr + (i as u64) * 4096, r.frames[i], writable);
    }
    r.npages
}

/// Free region `idx` (returns its frames to the PMM) WITHOUT refunding a budget.
/// Only for the orphan path in `sys_shm_create` (region created but no handle could
/// be installed) — the caller refunds the budget itself. Normal reclamation goes
/// through `decref`.
pub fn free(idx: u8) {
    let mut regs = REGIONS.lock();
    let r = &mut regs[idx as usize];
    if r.in_use {
        for &p in &r.frames[..r.npages] {
            crate::mm::pmm::free_frame(p);
        }
        r.in_use = false;
        r.rc = 0;
        r.npages = 0;
    }
}

/// Record the Memory budget that paid for region `idx`, so `decref` can refund it
/// when the last handle drops. Set once, right after a successful handle install.
pub fn set_mem(idx: u8, mem_idx: u8) {
    let mut regs = REGIONS.lock();
    let r = &mut regs[idx as usize];
    if r.in_use {
        r.mem_idx = mem_idx;
    }
}

/// Add a handle reference (an `alloc_slot`/`install` of an `ObjectRef::Shm(idx)`).
pub fn incref(idx: u8) {
    let mut regs = REGIONS.lock();
    let r = &mut regs[idx as usize];
    if r.in_use {
        r.rc = r.rc.saturating_add(1);
    }
}

/// Drop a handle reference. When the count hits zero, return the frames to the PMM
/// and refund the owning Memory budget. Safe to call for a not-in-use idx (no-op).
pub fn decref(idx: u8) {
    // Extract-then-release: don't hold REGIONS while touching the mem budget.
    let (freed, mem_idx, bytes) = {
        let mut regs = REGIONS.lock();
        let r = &mut regs[idx as usize];
        if !r.in_use {
            return;
        }
        if r.rc > 0 {
            r.rc -= 1;
        }
        if r.rc != 0 {
            return;
        }
        let bytes = (r.npages as u64) * 4096;
        let mem_idx = r.mem_idx;
        for &p in &r.frames[..r.npages] {
            crate::mm::pmm::free_frame(p);
        }
        r.in_use = false;
        r.npages = 0;
        (true, mem_idx, bytes)
    };
    if freed && bytes > 0 {
        crate::mm::mem::credit(mem_idx, bytes);
    }
}
