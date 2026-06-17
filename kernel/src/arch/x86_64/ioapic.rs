//! I/O APIC (§69 SMP Phase 2c) — routes device IRQs to LAPIC(s), the modern
//! replacement for the 8259 PIC's virtual-wire delivery. We program redirection
//! entries for the ISA IRQs the system uses (keyboard, mouse, serial) → the BSP's
//! LAPIC, and mask those lines on the PIC so they arrive ONCE, through the IOAPIC.
//!
//! PCI IRQs (the NIC) keep arriving via the PIC's virtual wire for now — routing
//! those through the IOAPIC needs the ACPI _PRT / PCI INTx→GSI map, a later step.
//!
//! Registers are accessed indirectly: write a register index to IOREGSEL (0x00),
//! then read/write its value at IOWIN (0x10). Each GSI `n` has a 64-bit
//! redirection entry at index `0x10 + 2n` (low) / `0x11 + 2n` (high).
//!
//! For QEMU q35 the IOAPIC sits at the standard base and ISA IRQ == GSI (no MADT
//! interrupt-source-override applies to lines 1/4/12), so we use them directly.

const IOAPIC_PHYS: u64 = 0xFEC0_0000;
const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
const REDIR_MASK: u32 = 1 << 16;

static mut IOAPIC_VBASE: u64 = 0;

#[inline]
unsafe fn reg_read(index: u32) -> u32 {
    core::ptr::write_volatile((IOAPIC_VBASE + IOREGSEL) as *mut u32, index);
    core::ptr::read_volatile((IOAPIC_VBASE + IOWIN) as *const u32)
}
#[inline]
unsafe fn reg_write(index: u32, val: u32) {
    core::ptr::write_volatile((IOAPIC_VBASE + IOREGSEL) as *mut u32, index);
    core::ptr::write_volatile((IOAPIC_VBASE + IOWIN) as *mut u32, val);
}

#[inline]
fn redir_lo(gsi: u8) -> u32 {
    0x10 + 2 * gsi as u32
}

/// Map the IOAPIC MMIO page into the kernel higher half (uncacheable). BSP only.
pub fn init() {
    unsafe {
        if IOAPIC_VBASE == 0 {
            let virt = crate::mm::phys_to_virt(IOAPIC_PHYS);
            crate::mm::vm::map_mmio_kernel_4k_in(crate::arch::current_cr3(), virt, IOAPIC_PHYS);
            IOAPIC_VBASE = virt;
        }
    }
}

/// Program GSI `gsi` to deliver `vector` to `dest_lapic` — fixed delivery,
/// physical destination, edge-triggered, active-high — and leave it MASKED. The
/// owning driver unmasks via its first `irq::ack` (matching the old PIC flow).
pub fn route(gsi: u8, vector: u8, dest_lapic: u8) {
    unsafe {
        // High dword: destination LAPIC id in bits 56..63 (so 24..31 of the high word).
        reg_write(redir_lo(gsi) + 1, (dest_lapic as u32) << 24);
        // Low dword: vector + masked (all other fields 0 = fixed/phys/edge/high).
        reg_write(redir_lo(gsi), vector as u32 | REDIR_MASK);
    }
}

/// Mask GSI `gsi` (so it can't re-fire while the driver processes it).
pub fn mask(gsi: u8) {
    unsafe {
        let r = redir_lo(gsi);
        reg_write(r, reg_read(r) | REDIR_MASK);
    }
}

/// Unmask GSI `gsi` (re-arm for the next interrupt).
pub fn unmask(gsi: u8) {
    unsafe {
        let r = redir_lo(gsi);
        reg_write(r, reg_read(r) & !REDIR_MASK);
    }
}

/// Lines we route through the IOAPIC (keyboard/serial/mouse). Everything else
/// (the NIC's PCI line) still goes through the PIC's virtual wire. Lets the
/// shared mask/eoi/ack paths pick the right controller per line.
pub fn routed(line: u8) -> bool {
    matches!(line, 1 | 4 | 12)
}
