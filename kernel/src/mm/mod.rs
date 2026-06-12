//! Memory management.
//!
//! v0 keeps this deliberately small (ABI law L6, v0 simplification): a bump
//! frame allocator over the largest usable region, and the HHDM offset that
//! lets the kernel reach any physical frame at `HHDM_OFFSET + phys`.
pub mod pmm;
pub mod vm;

use core::sync::atomic::{AtomicU64, Ordering};

/// Limine's higher-half direct map offset: `virt = phys + HHDM_OFFSET` for all
/// physical RAM. Captured once at boot, before anything else needs it.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Record the HHDM offset reported by the bootloader. Call once, early.
pub fn set_hhdm_offset(offset: u64) {
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
}

/// The HHDM offset captured at boot.
pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

/// Kernel-virtual address of a physical address, via the HHDM.
pub fn phys_to_virt(phys: u64) -> u64 {
    phys + hhdm_offset()
}
