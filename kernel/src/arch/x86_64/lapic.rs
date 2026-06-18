//! Local APIC (§69 SMP Phase 2). Each CPU has a LAPIC — the modern per-CPU
//! interrupt controller that replaces the legacy 8259 PIC and will drive the
//! per-CPU scheduler timer (Phase 2b) and inter-CPU IPIs (Phase 6).
//!
//! Phase 2a just *enables* the LAPIC on the BSP without changing how interrupts
//! are delivered: we put the LAPIC in **virtual-wire mode** (LINT0 = ExtINT) so
//! the 8259 PIC's interrupts (timer/keyboard/…) still pass through exactly as
//! before — enabling the LAPIC with LINT0 left masked would silently cut off the
//! PIT timer and freeze the scheduler.
//!
//! The registers are MMIO at the physical base in IA32_APIC_BASE; we reach them
//! through the HHDM, which is uncacheable for that region by the MTRRs.
use x86_64::registers::model_specific::Msr;

const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11; // xAPIC global enable

// Register offsets (bytes from the LAPIC base).
const REG_ID: usize = 0x020;
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0; // spurious-interrupt vector register
const REG_LINT0: usize = 0x350; // LVT LINT0
const REG_LINT1: usize = 0x360; // LVT LINT1

const SVR_ENABLE: u32 = 1 << 8; // APIC software enable
const LVT_MASKED: u32 = 1 << 16;
const DELIVERY_NMI: u32 = 0b100 << 8;
const DELIVERY_EXTINT: u32 = 0b111 << 8;

/// Spurious-interrupt vector we point the LAPIC at; its IDT handler is a no-op.
pub const SPURIOUS_VECTOR: u8 = 0xFF;
/// Local vector the LAPIC timer fires on (above the remapped PIC range 0x20-0x2F;
/// a LOCAL interrupt, not a PIC IRQ). §69 Phase 2b.
pub const TIMER_VECTOR: u8 = 0x30;

/// Virtual address of the LAPIC MMIO window. Same physical base on every CPU, so
/// the HHDM mapping is shared — set once on the BSP, reused by the APs.
static mut LAPIC_VBASE: u64 = 0;

#[inline]
unsafe fn read(off: usize) -> u32 {
    core::ptr::read_volatile((LAPIC_VBASE + off as u64) as *const u32)
}
#[inline]
unsafe fn write(off: usize, val: u32) {
    core::ptr::write_volatile((LAPIC_VBASE + off as u64) as *mut u32, val);
}

/// Enable the LAPIC on the current CPU in virtual-wire mode. Records the MMIO
/// base (idempotent — same on all CPUs). Returns this CPU's LAPIC id.
pub fn enable() -> u32 {
    let mut base_msr = Msr::new(IA32_APIC_BASE);
    let base = unsafe { base_msr.read() };
    let phys = base & 0xffff_f000;
    unsafe {
        if LAPIC_VBASE == 0 {
            // The HHDM does NOT cover MMIO holes like 0xFEE00000, so map the LAPIC
            // page explicitly (uncacheable). It goes in the kernel higher-half whose
            // PML4 entries every user address space shares (new_user_pml4 copies
            // 256..512), so interrupt handlers reach it in any process context.
            let virt = crate::mm::phys_to_virt(phys);
            crate::mm::vm::map_mmio_kernel_4k_in(crate::arch::current_cr3(), virt, phys);
            LAPIC_VBASE = virt;
        }
        base_msr.write(base | APIC_BASE_ENABLE); // ensure xAPIC globally enabled
        // Software-enable + spurious vector.
        write(REG_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);
        // Virtual-wire: LINT0 forwards the 8259 PIC (ExtINT), LINT1 = NMI. Without
        // this the PIT timer would be cut off and the scheduler would freeze.
        write(REG_LINT0, DELIVERY_EXTINT);
        write(REG_LINT1, DELIVERY_NMI);
        let _ = LVT_MASKED; // (used when we later mask these for IOAPIC mode)
        read(REG_ID) >> 24
    }
}

/// Signal end-of-interrupt to the LAPIC (for LAPIC-delivered IRQs; Phase 2b+).
#[inline]
pub unsafe fn eoi() {
    write(REG_EOI, 0);
}

// --- IPI (§75: stop-other-CPUs-on-panic, mirroring FreeBSD stop_cpus_hard) ---
const REG_ICR_LOW: usize = 0x300;
/// Delivery mode NMI in the ICR (bits 8..10 = 0b100).
const ICR_DELIVERY_NMI: u32 = 0b100 << 8;
/// Destination shorthand "all excluding self" (bits 18..19 = 0b11).
const ICR_DEST_ALL_BUT_SELF: u32 = 0b11 << 18;

/// Broadcast an **NMI** to every other CPU. NMI is non-maskable, so it lands even
/// on a core spinning with IF=0 or wedged in a fault handler — that's the point
/// (a maskable IPI wouldn't reach a stuck core). Used by the panic path to halt
/// the other cores before printing, so they can't corrupt more state or hold the
/// console lock. The vector field is ignored for NMI delivery.
pub unsafe fn send_nmi_all_but_self() {
    if LAPIC_VBASE == 0 {
        return; // LAPIC not up yet (very early boot) — nothing to stop
    }
    write(REG_ICR_LOW, ICR_DELIVERY_NMI | ICR_DEST_ALL_BUT_SELF);
}

// --- LAPIC timer (§69 Phase 2b) — the per-CPU scheduler tick ----------------
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CUR: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;
const TIMER_PERIODIC: u32 = 1 << 17;
const TIMER_MASKED: u32 = 1 << 16;
const TIMER_DIV_16: u32 = 0b0011; // divide the timer input clock by 16

/// Calibrate the LAPIC timer against PIT channel 2 (polled, no IRQs needed) and
/// start it in PERIODIC mode at `hz`, delivering on `vector`. Replaces the PIT as
/// the scheduler tick. The per-CPU count is cached so APs can start their timer
/// without re-calibrating.
static mut TIMER_COUNT: u32 = 0;

pub fn start_timer(vector: u8, hz: u32) {
    use x86_64::instructions::port::Port;
    unsafe {
        write(REG_TIMER_DIV, TIMER_DIV_16);

        if TIMER_COUNT == 0 {
            // --- calibrate: count LAPIC ticks during one polled 10 ms PIT ch2 wait ---
            let pit_10ms: u16 = (1_193_182u32 / 100) as u16; // ticks in 10 ms
            let mut p61 = Port::<u8>::new(0x61);
            // gate on, speaker off
            let v = p61.read();
            p61.write((v & 0xFC) | 0x01);
            Port::<u8>::new(0x43).write(0xB0); // ch2, lo/hi byte, mode 0, binary
            let mut ch2 = Port::<u8>::new(0x42);
            ch2.write((pit_10ms & 0xFF) as u8);
            ch2.write((pit_10ms >> 8) as u8);
            // Toggle the gate to start the countdown cleanly, then immediately start
            // the LAPIC counting down from max in one-shot.
            let g = p61.read() & 0xFE;
            p61.write(g);
            p61.write(g | 0x01); // gate high -> ch2 starts counting
            write(REG_LVT_TIMER, TIMER_MASKED); // one-shot, masked (calibration only)
            write(REG_TIMER_INIT, 0xFFFF_FFFF);
            while p61.read() & 0x20 == 0 {} // wait for ch2 OUT high = terminal count
            // LAPIC ticks in 10 ms == the initial count for a 100 Hz periodic timer.
            TIMER_COUNT = 0xFFFF_FFFFu32 - read(REG_TIMER_CUR);
        }

        // The period scales inversely with frequency: count = (count@100Hz) * 100/hz.
        let count = ((TIMER_COUNT as u64 * 100) / hz.max(1) as u64) as u32;
        write(REG_LVT_TIMER, TIMER_PERIODIC | vector as u32);
        write(REG_TIMER_INIT, count.max(1));
    }
}
