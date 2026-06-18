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
/// Max pages per region: 1024 * 4 KiB = 4 MiB — enough for a 1280x800x4 shared
/// framebuffer (1000 pages), the gpu/oxcomp scanout buffer, as well as client
/// wl_shm buffers.
const MAX_PAGES: usize = 1024;

#[derive(Clone, Copy)]
struct Region {
    in_use: bool,
    npages: usize,
    frames: [u64; MAX_PAGES],
}
impl Region {
    const fn new() -> Self {
        Region { in_use: false, npages: 0, frames: [0; MAX_PAGES] }
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

/// Free region `idx` (returns its frames to the PMM). Reset in place.
#[allow(dead_code)]
pub fn free(idx: u8) {
    let mut regs = REGIONS.lock();
    let r = &mut regs[idx as usize];
    if r.in_use {
        for &p in &r.frames[..r.npages] {
            crate::mm::pmm::free_frame(p);
        }
        r.in_use = false;
        r.npages = 0;
    }
}
