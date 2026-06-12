//! User-pointer validation (ABI §4.1: a bad user pointer yields `E_FAULT`).
//!
//! v0 shares one address space (D1) and accesses user memory directly, so before
//! the kernel touches a user pointer it walks the page tables to confirm every
//! page in range is present and USER-accessible (and writable, if needed). A
//! kernel/higher-half pointer is rejected by the lower-half range check.
use oxbow_abi::SysError;
use x86_64::registers::control::Cr3;

use crate::mm;

const LOWER_HALF_END: u64 = 0x0000_8000_0000_0000;
const PAGE: u64 = 4096;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Validate `[ptr, ptr+len)` as user-accessible memory. `need_write` also
/// requires it be writable. Empty ranges are trivially OK.
pub fn check_user(ptr: u64, len: usize, need_write: bool) -> Result<(), SysError> {
    if len == 0 {
        return Ok(());
    }
    let end = ptr.checked_add(len as u64).ok_or(SysError::Fault)?;
    if ptr >= LOWER_HALF_END || end > LOWER_HALF_END {
        return Err(SysError::Fault);
    }
    let mut page = ptr & !(PAGE - 1);
    while page < end {
        if !walk_user(page, need_write) {
            return Err(SysError::Fault);
        }
        page += PAGE;
    }
    Ok(())
}

/// Walk the active page tables for `virt`, requiring PRESENT + USER (and
/// WRITABLE if `need_write`) at every level. Returns false on any failure.
fn walk_user(virt: u64, need_write: bool) -> bool {
    let (l4_frame, _) = Cr3::read();
    let mut table_phys = l4_frame.start_address().as_u64();

    let indices = [
        (virt >> 39) & 0x1ff,
        (virt >> 30) & 0x1ff,
        (virt >> 21) & 0x1ff,
        (virt >> 12) & 0x1ff,
    ];

    for (level, &idx) in indices.iter().enumerate() {
        let table = unsafe { &*(mm::phys_to_virt(table_phys) as *const [u64; 512]) };
        let e = table[idx as usize];
        if e & PRESENT == 0 || e & USER == 0 {
            return false;
        }
        if need_write && e & WRITABLE == 0 {
            return false;
        }
        // A huge page at PDPT (level 1) or PD (level 2) is a leaf.
        if (level == 1 || level == 2) && (e & HUGE != 0) {
            return true;
        }
        table_phys = e & ADDR_MASK;
    }
    true
}
