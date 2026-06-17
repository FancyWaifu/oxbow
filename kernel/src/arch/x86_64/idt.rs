//! IDT — CPU exceptions plus the v1 hardware IRQ vectors.
//!
//! v0 installed exception handlers only and masked the PICs. v1 remaps the PICs
//! to 0x20-0x2F and adds the timer (IRQ0 → vector 0x20), spurious-IRQ, and a
//! loud catch-all for the lines we leave masked. Exception handlers still dump
//! and panic (except #BP, which resumes) so bugs stay readable. All gates are
//! interrupt gates (IF cleared on entry) — the kernel is never preemptible.
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt::DOUBLE_FAULT_IST_INDEX;
use super::pic;
use crate::println;

/// Generate a bindable PCI-device IRQ handler for one line: mask the line, EOI
/// in-kernel, then signal the bound notification — the standard discipline. We
/// install one per common PCI line (5/9/10/11) so a NIC works whatever IRQ the
/// firmware routed it to (QEMU q35 picks 11; a Proxmox i440fx VM may pick 10).
macro_rules! pci_irq {
    ($name:ident, $line:literal) => {
        extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
            pic::mask($line);
            pic::eoi($line);
            crate::irq::fire($line);
        }
    };
}
pci_irq!(pci_irq5, 5);
pci_irq!(pci_irq9, 9);
pci_irq!(pci_irq10, 10);
pci_irq!(pci_irq11, 11);

/// §69 Phase 2c: an ISA IRQ delivered through the IOAPIC → LAPIC (not the PIC).
/// Mask the IOAPIC redirection entry, EOI the LAPIC, then signal the bound notif.
macro_rules! ioapic_irq {
    ($name:ident, $line:literal) => {
        extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
            super::ioapic::mask($line);
            unsafe { super::lapic::eoi() };
            crate::irq::fire($line);
        }
    };
}
ioapic_irq!(mouse_irq, 12); // IRQ12 — i8042 PS/2 mouse, routed via the IOAPIC

/// Vector the PIT timer (IRQ0) lands on after the PIC remap.
const TIMER_VECTOR: u8 = 0x20;

/// Monotonic tick counter, incremented by the timer handler.
pub static TICKS: AtomicU64 = AtomicU64::new(0);

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

/// Install the IDT, then remap + fully mask the PICs. Lines are unmasked later
/// (the scheduler arms IRQ0); nothing fires until interrupts are enabled.
pub fn init() {
    unsafe {
        let idt = &mut *addr_of_mut!(IDT);
        idt.divide_error.set_handler_fn(divide_error);
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.general_protection_fault.set_handler_fn(general_protection_fault);
        idt.page_fault.set_handler_fn(page_fault);
        idt.double_fault
            .set_handler_fn(double_fault)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);

        // Hardware IRQ vectors (PIC remapped base 0x20).
        idt[TIMER_VECTOR].set_handler_fn(timer); // IRQ0 — scheduler tick
        idt[0x21].set_handler_fn(keyboard); // IRQ1 — i8042 keyboard (bindable)
        idt[0x24].set_handler_fn(serial_com1); // IRQ4 — 16550 COM1 RX (bindable)
        idt[0x25].set_handler_fn(pci_irq5); // IRQ5  — PCI INTx (bindable)
        idt[0x27].set_handler_fn(spurious_master); // IRQ7 spurious: no EOI
        idt[0x29].set_handler_fn(pci_irq9); // IRQ9   — PCI INTx (bindable)
        idt[0x2A].set_handler_fn(pci_irq10); // IRQ10 — PCI INTx (bindable)
        idt[0x2B].set_handler_fn(pci_irq11); // IRQ11 — PCI INTx, e1000 on QEMU q35
        idt[0x2C].set_handler_fn(mouse_irq); // IRQ12 — i8042 PS/2 mouse (bindable)
        idt[0x2F].set_handler_fn(spurious_slave); // IRQ15 spurious: EOI master
        idt[super::lapic::SPURIOUS_VECTOR].set_handler_fn(lapic_spurious); // §69 LAPIC
        idt[super::lapic::TIMER_VECTOR].set_handler_fn(lapic_timer); // §69 LAPIC timer
        for v in 0x22u8..=0x2E {
            if !matches!(v, 0x24 | 0x25 | 0x27 | 0x29 | 0x2A | 0x2B | 0x2C) {
                idt[v].set_handler_fn(unexpected_irq);
            }
        }

        let idt_ref: &'static InterruptDescriptorTable = &*addr_of!(IDT);
        idt_ref.load();
    }
    pic::remap();
    pic::mask_all();
}

/// Count of times the timer has preempted ring 3 (user), for the Phase-5
/// checkpoint message.

extern "x86-interrupt" fn timer(frame: InterruptStackFrame) {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // EOI BEFORE the (possible) context switch: if we switched first, the PIC's
    // in-service bit would block all further IRQ0s until this exact thread
    // resumes — a silent scheduler freeze. Safe here because IF=0 in the gate.
    pic::eoi(0);

    // ~1 Hz: signal the tick notification from IRQ context (wake-only, no block).
    if tick % 100 == 0 {
        crate::notif::fire_tick();
    }

    // Wake any thread whose timed-wait deadline has elapsed (sys_chan_wait timeout).
    crate::thread::wake_expired(tick);

    // (Ring-3 preemptibility was proven in the threads arc; the per-preempt
    // trace print is retired — it would otherwise corrupt the serial console,
    // which the shell now shares with kernel output.)
    let _ = frame;
    crate::thread::preempt();
}

/// §69 Phase 2b: the LAPIC timer is the scheduler tick once the PIT (IRQ0) is
/// retired. Same work as `timer()` but EOI's the LAPIC (a local interrupt), not
/// the PIC. EOI before the context switch for the same in-service-bit reason.
extern "x86-interrupt" fn lapic_timer(frame: InterruptStackFrame) {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    unsafe { super::lapic::eoi() };
    if tick % 100 == 0 {
        crate::notif::fire_tick();
    }
    crate::thread::wake_expired(tick);
    let _ = frame;
    crate::thread::preempt();
}

extern "x86-interrupt" fn keyboard(_frame: InterruptStackFrame) {
    // §69 Phase 2c: IRQ1 now arrives via the IOAPIC → LAPIC. Mask the IOAPIC
    // redirection entry (so it can't re-fire), EOI the LAPIC, then signal the
    // bound notification. The driver drains the i8042 and acks (unmask).
    super::ioapic::mask(1);
    unsafe { super::lapic::eoi() };
    crate::irq::fire(1);
}

extern "x86-interrupt" fn serial_com1(_frame: InterruptStackFrame) {
    // IRQ4 — COM1 RX, also via the IOAPIC → LAPIC. Mask, EOI the LAPIC, signal.
    super::ioapic::mask(4);
    unsafe { super::lapic::eoi() };
    crate::irq::fire(4);
}

extern "x86-interrupt" fn spurious_master(_frame: InterruptStackFrame) {
    // IRQ7 spurious interrupt — must NOT be EOI'd.
}

extern "x86-interrupt" fn lapic_spurious(_frame: InterruptStackFrame) {
    // §69: LAPIC spurious interrupt — by design, requires no EOI.
}

extern "x86-interrupt" fn spurious_slave(_frame: InterruptStackFrame) {
    // IRQ15 spurious — the slave is spurious, so EOI only the master cascade.
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(0x20).write(0x20);
    }
}

extern "x86-interrupt" fn unexpected_irq(_frame: InterruptStackFrame) {
    panic!("unexpected hardware IRQ on a masked line");
}

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    println!(
        "[trap] #BP at rip={:#x} -- resuming",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    panic!("#DE divide error\n{:#?}", frame);
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    panic!(
        "#UD invalid opcode at rip={:#x}\n{:#?}",
        frame.instruction_pointer.as_u64(),
        frame
    );
}

extern "x86-interrupt" fn general_protection_fault(frame: InterruptStackFrame, error_code: u64) {
    // A fault from ring 3 kills the offending thread/process; the kernel lives on.
    if frame.code_segment.0 & 3 == 3 {
        println!(
            "[trap] #GP user err={:#x} -- killing tcb {} (proc {})",
            error_code,
            crate::thread::current(),
            crate::thread::current_proc()
        );
        crate::thread::kill_current_user();
    }
    panic!(
        "#GP (kernel) error_code={:#x} at rip={:#x}\n{:#?}",
        error_code,
        frame.instruction_pointer.as_u64(),
        frame
    );
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let cr2 = x86_64::registers::control::Cr2::read();
    // A user-mode page fault kills the offending thread/process, not the machine.
    if error_code.contains(PageFaultErrorCode::USER_MODE) {
        println!(
            "[trap] #PF user cr2={:?} err={:?} rip={:#x} -- killing tcb {} (proc {})",
            cr2,
            error_code,
            frame.instruction_pointer.as_u64(),
            crate::thread::current(),
            crate::thread::current_proc()
        );
        crate::thread::kill_current_user();
    }
    panic!(
        "#PF (kernel) accessing {:?}, error={:?}, at rip={:#x}\n{:#?}",
        cr2,
        error_code,
        frame.instruction_pointer.as_u64(),
        frame
    );
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    panic!("#DF error_code={:#x}\n{:#?}", error_code, frame);
}
