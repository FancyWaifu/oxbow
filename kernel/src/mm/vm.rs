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
