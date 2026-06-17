//! Virtual memory — build the kernel's own page tables and leave Limine's behind.
//!
//! v0 uses a SINGLE address space (D1): the kernel maps its image in the higher
//! half (U=0) and the full HHDM, then later phases add the one user process's
//! pages (U=1) into this same table. Every mapping flows through `map_page`,
//! which asserts `!(WRITABLE && EXECUTABLE)` — the one chokepoint that enforces
//! ABI law L4 (W^X) for the whole system.
use limine::memory_map::EntryType;
use limine::response::MemoryMapResponse;
use x86_64::registers::control::{Cr3, Efer, EferFlags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags as Flags, PhysFrame,
    Size2MiB, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use super::pmm;

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

// Section boundaries from linker.ld.
extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
}

fn sym(s: &'static u8) -> u64 {
    s as *const u8 as u64
}

/// Adapts the bump PMM to the `x86_64` crate's frame-allocator trait, so the
/// mapper can allocate intermediate page tables.
struct PmmAlloc;
unsafe impl FrameAllocator<Size4KiB> for PmmAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        pmm::alloc_frame().map(|p| PhysFrame::containing_address(PhysAddr::new(p)))
    }
}

fn align_up(x: u64, align: u64) -> u64 {
    (x + align - 1) & !(align - 1)
}
fn align_down(x: u64, align: u64) -> u64 {
    x & !(align - 1)
}

/// Build a fresh PML4 mapping the kernel image (W^X-clean) and the HHDM, and
/// return its physical address for loading into CR3.
pub fn init(memmap: &MemoryMapResponse, kernel_phys_base: u64, kernel_virt_base: u64) -> u64 {
    // NX bits in PTEs fault as reserved-bit violations unless EFER.NXE is set.
    // Limine sets it, but make it explicit since W^X depends on it.
    unsafe {
        Efer::update(|f| {
            f.insert(EferFlags::NO_EXECUTE_ENABLE);
        });
    }

    let hhdm = VirtAddr::new(super::hhdm_offset());

    let pml4_phys = pmm::alloc_frame().expect("vm: no frame for PML4");
    // Frame is zeroed by the PMM; reinterpret it through the (current Limine) HHDM.
    let pml4: &mut PageTable = unsafe { &mut *(super::phys_to_virt(pml4_phys) as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(pml4, hhdm) };
    let mut falloc = PmmAlloc;

    // 1) HHDM: map all RAM-ish physical memory at hhdm+phys, RW + NX, 2 MiB pages.
    //    Bounded by the highest address among regions we actually touch.
    let mut max_phys = 0u64;
    for e in memmap.entries() {
        let touched = e.entry_type == EntryType::USABLE
            || e.entry_type == EntryType::BOOTLOADER_RECLAIMABLE
            || e.entry_type == EntryType::EXECUTABLE_AND_MODULES;
        if touched {
            let end = e.base + e.length;
            if end > max_phys {
                max_phys = end;
            }
        }
    }
    let hhdm_end = align_up(max_phys, PAGE_2M);
    let mut phys = 0u64;
    while phys < hhdm_end {
        let page = Page::<Size2MiB>::containing_address(VirtAddr::new(hhdm.as_u64() + phys));
        let frame = PhysFrame::<Size2MiB>::containing_address(PhysAddr::new(phys));
        let flags = Flags::PRESENT | Flags::WRITABLE | Flags::NO_EXECUTE;
        debug_assert!(!(flags.contains(Flags::WRITABLE) && !flags.contains(Flags::NO_EXECUTE)));
        unsafe {
            mapper
                .map_to(page, frame, flags, &mut falloc)
                .expect("vm: hhdm map_to")
                .ignore(); // whole CR3 is reloaded; no per-page TLB flush needed
        }
        phys += PAGE_2M;
    }

    // 2) Kernel image, per-section, 4 KiB pages, with exact W^X-clean perms.
    let rx = Flags::PRESENT; // read + execute (no WRITABLE, no NO_EXECUTE)
    let ro = Flags::PRESENT | Flags::NO_EXECUTE; // read-only data
    let rw = Flags::PRESENT | Flags::WRITABLE | Flags::NO_EXECUTE; // data/bss

    map_kernel_section(
        &mut mapper,
        &mut falloc,
        sym(unsafe { &__text_start }),
        sym(unsafe { &__text_end }),
        rx,
        kernel_phys_base,
        kernel_virt_base,
    );
    map_kernel_section(
        &mut mapper,
        &mut falloc,
        sym(unsafe { &__rodata_start }),
        sym(unsafe { &__rodata_end }),
        ro,
        kernel_phys_base,
        kernel_virt_base,
    );
    map_kernel_section(
        &mut mapper,
        &mut falloc,
        sym(unsafe { &__data_start }),
        sym(unsafe { &__data_end }),
        rw,
        kernel_phys_base,
        kernel_virt_base,
    );

    pml4_phys
}

/// Create a fresh per-process PML4 that SHARES the kernel's upper half. We
/// raw-copy entries 256..512 of the current PML4 — those point at the same
/// PDPT frames, so the kernel image (slot 511), the HHDM (slot 256), and every
/// kernel stack live in `.bss` are present in every address space. The low half
/// (user space) starts empty. Invariant for arc 2+: the kernel never creates a
/// NEW upper-half PML4 entry after `init`, so this snapshot stays complete.
pub fn new_user_pml4() -> u64 {
    let frame = pmm::alloc_frame().expect("vm: no frame for user PML4"); // zeroed
    let new_pml4 = unsafe { &mut *(super::phys_to_virt(frame) as *mut PageTable) };

    let (cur_frame, _) = Cr3::read();
    let cur_pml4 =
        unsafe { &*(super::phys_to_virt(cur_frame.start_address().as_u64()) as *const PageTable) };

    for i in 256..512 {
        new_pml4[i] = cur_pml4[i].clone();
    }
    frame
}

/// Free every frame reachable from the LOWER half (user, entries 0..256) of the
/// address space rooted at `pml4_phys`: the leaf data frames and the intermediate
/// page-table frames, then the PML4 itself. The upper half (256..512) is the
/// SHARED kernel image + HHDM and is never touched. Shared user frames (Frame
/// objects, zero-copy shmem) are skipped so they aren't double-freed by a peer's
/// teardown — they leak until a future Frame-refcount arc.
///
/// MUST NOT run while this address space is the live CR3 — call it only after the
/// owning thread has switched away (we free on slot reuse, never on the dying
/// thread itself).
pub fn free_user_pml4(pml4_phys: u64) {
    unsafe {
        let l4 = &*(super::phys_to_virt(pml4_phys) as *const PageTable);
        for i in 0..256 {
            let e4 = &l4[i];
            if !e4.flags().contains(Flags::PRESENT) {
                continue;
            }
            let l3p = e4.addr().as_u64();
            let l3 = &*(super::phys_to_virt(l3p) as *const PageTable);
            for j in 0..512 {
                let e3 = &l3[j];
                if !e3.flags().contains(Flags::PRESENT) || e3.flags().contains(Flags::HUGE_PAGE) {
                    continue;
                }
                let l2p = e3.addr().as_u64();
                let l2 = &*(super::phys_to_virt(l2p) as *const PageTable);
                for k in 0..512 {
                    let e2 = &l2[k];
                    if !e2.flags().contains(Flags::PRESENT) || e2.flags().contains(Flags::HUGE_PAGE)
                    {
                        continue;
                    }
                    let l1p = e2.addr().as_u64();
                    let l1 = &*(super::phys_to_virt(l1p) as *const PageTable);
                    for m in 0..512 {
                        let e1 = &l1[m];
                        if !e1.flags().contains(Flags::PRESENT) {
                            continue;
                        }
                        let leaf = e1.addr().as_u64();
                        if super::mem::is_shared_frame(leaf) {
                            // A shared Frame: drop this mapping's reference; the
                            // frame is freed when the last mapper tears down.
                            super::mem::frame_unmap(leaf);
                        } else {
                            pmm::free_frame(leaf);
                        }
                    }
                    pmm::free_frame(l1p); // the PT
                }
                pmm::free_frame(l2p); // the PD
            }
            pmm::free_frame(l3p); // the PDPT
        }
    }
    pmm::free_frame(pml4_phys);
}

/// Boot self-test (canary for the share-copy): create a second address space,
/// switch CR3 into it, execute kernel code + read through the HHDM under it,
/// then switch back. Must run with IF=0 (no timer yet). Stays as a permanent
/// boot assert — a missed upper-half slot here triple-faults instead of later.
pub fn as_hop_selftest() {
    let as1 = new_user_pml4();
    crate::println!("[vm] as#1 pml4={:#x} (upper 256 slots shared)", as1);

    let (boot_frame, flags) = Cr3::read();
    let as1_frame = PhysFrame::containing_address(PhysAddr::new(as1));

    // A sentinel in physical RAM, reached via the HHDM under both address spaces.
    let probe = pmm::alloc_frame().expect("vm: no probe frame");
    let probe_va = super::phys_to_virt(probe) as *mut u64;
    let ok_in;
    unsafe {
        probe_va.write_volatile(0xCAFE_BABE_DEAD_BEEF);
        Cr3::write(as1_frame, flags); // hop into the new address space
        ok_in = probe_va.read_volatile() == 0xCAFE_BABE_DEAD_BEEF; // HHDM read under as1
        Cr3::write(boot_frame, flags); // hop back
    }
    crate::println!(
        "[vm] cr3 hop: in -- {}; back -- alive",
        if ok_in { "alive" } else { "FAIL" }
    );
}

/// Physical address of the currently-loaded PML4 (the live address space).
#[allow(dead_code)] // handy AS helper; not currently on the boot path
pub fn current_pml4() -> u64 {
    Cr3::read().0.start_address().as_u64()
}

/// Map one 4 KiB user page into the address space rooted at `pml4_phys` (which
/// need NOT be the live one — used to populate a process before it runs).
/// USER_ACCESSIBLE is set on the leaf only; the `x86_64` crate propagates it to
/// the parent tables. Goes through the W^X assert like every mapping (ABI L4).
pub fn map_user_4k_in(pml4_phys: u64, virt: u64, phys: u64, writable: bool, executable: bool) {
    assert!(!(writable && executable), "W^X violation in user mapping");
    assert!(
        virt < 0x0000_8000_0000_0000,
        "user vaddr must be in the lower half"
    );

    let mut flags = Flags::PRESENT | Flags::USER_ACCESSIBLE;
    if writable {
        flags |= Flags::WRITABLE;
    }
    if !executable {
        flags |= Flags::NO_EXECUTE;
    }

    let hhdm = VirtAddr::new(super::hhdm_offset());
    let l4: &mut PageTable =
        unsafe { &mut *(super::phys_to_virt(pml4_phys) as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(l4, hhdm) };
    let mut falloc = PmmAlloc;

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut falloc)
            .expect("vm: map_user_4k_in")
            .ignore(); // target table may not be live; the CR3 load flushes it
    }
}

/// Map one 4 KiB MMIO page (device registers) into a user address space,
/// **uncacheable** (PCD) and NX — for a driver's PCI BAR. Writable always (device
/// registers are RW). The phys range is a device address, not RAM, so no frame
/// is consumed; only the page tables are.
pub fn map_mmio_4k_in(pml4_phys: u64, virt: u64, phys: u64) {
    assert!(virt < 0x0000_8000_0000_0000, "mmio vaddr must be lower half");
    let flags = Flags::PRESENT
        | Flags::USER_ACCESSIBLE
        | Flags::WRITABLE
        | Flags::NO_EXECUTE
        | Flags::NO_CACHE;
    let hhdm = VirtAddr::new(super::hhdm_offset());
    let l4: &mut PageTable = unsafe { &mut *(super::phys_to_virt(pml4_phys) as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(l4, hhdm) };
    let mut falloc = PmmAlloc;
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut falloc)
            .expect("vm: map_mmio_4k_in")
            .ignore();
    }
}

/// Like `map_mmio_4k_in` but into the KERNEL higher half (no USER bit) — for
/// per-CPU device registers (the LAPIC, later the IOAPIC). The HHDM does not cover
/// MMIO holes, and the higher-half PML4 entries are shared by every address space
/// (`new_user_pml4` copies 256..512), so the mapping is reachable from interrupt
/// context in any process. (§69 SMP)
pub fn map_mmio_kernel_4k_in(pml4_phys: u64, virt: u64, phys: u64) {
    assert!(virt >= 0xffff_8000_0000_0000, "kernel mmio vaddr must be higher half");
    let flags = Flags::PRESENT | Flags::WRITABLE | Flags::NO_EXECUTE | Flags::NO_CACHE;
    let hhdm = VirtAddr::new(super::hhdm_offset());
    let l4: &mut PageTable = unsafe { &mut *(super::phys_to_virt(pml4_phys) as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(l4, hhdm) };
    let mut falloc = PmmAlloc;
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut falloc)
            .expect("vm: map_mmio_kernel_4k_in")
            .flush();
    }
}

/// Change the protection of already-mapped user pages in `[vaddr, vaddr+pages)`.
/// The JIT primitive (`mmap` RW → write code → `mprotect` RX): it flips the
/// WRITABLE / NO_EXECUTE leaf flags. W^X (L4) still holds — `!(writable &&
/// executable)` is asserted, so no page is ever W and X at once; only the RW↔RX
/// transition is allowed. Operates on the live CR3, so each page is TLB-flushed.
/// Returns `Err` if any page in the range isn't mapped.
pub fn protect_user_range(
    pml4_phys: u64,
    vaddr: u64,
    pages: u64,
    writable: bool,
    executable: bool,
) -> Result<(), ()> {
    assert!(!(writable && executable), "W^X violation in protect");
    let mut flags = Flags::PRESENT | Flags::USER_ACCESSIBLE;
    if writable {
        flags |= Flags::WRITABLE;
    }
    if !executable {
        flags |= Flags::NO_EXECUTE;
    }
    let hhdm = VirtAddr::new(super::hhdm_offset());
    let l4: &mut PageTable = unsafe { &mut *(super::phys_to_virt(pml4_phys) as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(l4, hhdm) };
    for i in 0..pages {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr + i * 4096));
        match unsafe { mapper.update_flags(page, flags) } {
            Ok(flush) => flush.flush(),
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

const PTE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
const PTE_PRESENT: u64 = 1 << 0;
const PTE_HUGE: u64 = 1 << 7;

/// Walk the caller's tables over `[vaddr, vaddr+pages*4096)`. Returns the number
/// of intermediate page tables `map_to` would need to allocate for the range
/// (deduped — adjacent pages share tables), or `Err` if ANY page in range is
/// already present (overlap, incl. huge-page leaves). One read-only pass buys
/// both atomic overlap detection and the exact budget cost.
pub fn probe_user_range(pml4_phys: u64, vaddr: u64, pages: u64) -> Result<u64, ()> {
    let hhdm = super::hhdm_offset();
    let table = |phys: u64| -> &'static [u64; 512] {
        unsafe { &*((hhdm + phys) as *const [u64; 512]) }
    };

    let mut missing = 0u64;
    // Region keys of tables already counted as missing (so shared tables across
    // adjacent pages count once).
    let (mut seen_pdpt, mut seen_pd, mut seen_pt) = (u64::MAX, u64::MAX, u64::MAX);

    for p in 0..pages {
        let va = vaddr + p * 4096;
        let i = [
            ((va >> 39) & 0x1ff) as usize,
            ((va >> 30) & 0x1ff) as usize,
            ((va >> 21) & 0x1ff) as usize,
            ((va >> 12) & 0x1ff) as usize,
        ];

        let e0 = table(pml4_phys)[i[0]];
        if e0 & PTE_PRESENT == 0 {
            if va >> 39 != seen_pdpt { missing += 1; seen_pdpt = va >> 39; }
            if va >> 30 != seen_pd { missing += 1; seen_pd = va >> 30; }
            if va >> 21 != seen_pt { missing += 1; seen_pt = va >> 21; }
            continue;
        }
        let e1 = table(e0 & PTE_ADDR_MASK)[i[1]];
        if e1 & PTE_PRESENT == 0 {
            if va >> 30 != seen_pd { missing += 1; seen_pd = va >> 30; }
            if va >> 21 != seen_pt { missing += 1; seen_pt = va >> 21; }
            continue;
        }
        if e1 & PTE_HUGE != 0 {
            return Err(()); // 1 GiB huge-page overlap
        }
        let e2 = table(e1 & PTE_ADDR_MASK)[i[2]];
        if e2 & PTE_PRESENT == 0 {
            if va >> 21 != seen_pt { missing += 1; seen_pt = va >> 21; }
            continue;
        }
        if e2 & PTE_HUGE != 0 {
            return Err(()); // 2 MiB huge-page overlap
        }
        let e3 = table(e2 & PTE_ADDR_MASK)[i[3]];
        if e3 & PTE_PRESENT != 0 {
            return Err(()); // page already mapped
        }
        // leaf absent, all tables present -> nothing missing for this page
    }
    Ok(missing)
}

/// Map one anonymous 4 KiB page into the live caller AS and FLUSH it (unlike
/// `map_user_4k_in`'s `.ignore()`, valid there only because a CR3 load follows).
/// Anonymous pages are always NX (W^X — there is no executable mapping syscall).
pub fn map_user_4k_live(pml4_phys: u64, virt: u64, phys: u64, writable: bool) {
    let mut flags = Flags::PRESENT | Flags::USER_ACCESSIBLE | Flags::NO_EXECUTE;
    if writable {
        flags |= Flags::WRITABLE;
    }
    let hhdm = VirtAddr::new(super::hhdm_offset());
    let l4: &mut PageTable =
        unsafe { &mut *(super::phys_to_virt(pml4_phys) as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(l4, hhdm) };
    let mut falloc = PmmAlloc;
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut falloc)
            .expect("vm: map_user_4k_live")
            .flush();
    }
}

fn map_kernel_section(
    mapper: &mut OffsetPageTable,
    falloc: &mut PmmAlloc,
    vstart: u64,
    vend: u64,
    flags: Flags,
    kphys: u64,
    kvirt: u64,
) {
    // The W^X chokepoint (ABI L4): no kernel mapping is ever W and X together.
    assert!(
        !(flags.contains(Flags::WRITABLE) && !flags.contains(Flags::NO_EXECUTE)),
        "W^X violation in kernel mapping"
    );
    let mut v = align_down(vstart, PAGE_4K);
    let end = align_up(vend, PAGE_4K);
    while v < end {
        let phys = v - kvirt + kphys;
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(v));
        let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(phys));
        unsafe {
            mapper
                .map_to(page, frame, flags, falloc)
                .expect("vm: kernel map_to")
                .ignore();
        }
        v += PAGE_4K;
    }
}
