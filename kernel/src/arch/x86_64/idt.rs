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
/// EOI the LAPIC, then signal the bound notif. We deliberately do NOT mask the
/// IOAPIC line on fire (unlike the PIC pci_irq! path): the i8042 (and the 16550)
/// hold their IRQ line HIGH the whole time data is buffered — effectively
/// level-asserted — but the redirection entries are EDGE-triggered. If we masked
/// on fire, a byte arriving while masked would leave the line steady-high; the
/// later unmask produces no fresh low→high edge, so the interrupt is LOST and the
/// (shared, single-byte) i8042 output buffer wedges — freezing BOTH keyboard and
/// mouse. The PIC's IRR latched across a mask and hid this; the IOAPIC doesn't.
/// Not masking is safe here because the driver's drain() empties the entire
/// buffer every wake (so the line falls low and only a genuinely new byte re-edges
/// — no storm), and a stuck driver just leaves the line high with no new edges.
macro_rules! ioapic_irq {
    ($name:ident, $line:literal) => {
        extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
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
        // §77: #PF/#GP run on a dedicated IST stack so the handler survives a
        // corrupted thread kernel stack (otherwise it re-faults silently).
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault)
            .set_stack_index(super::gdt::FAULT_IST_INDEX);
        idt.page_fault
            .set_handler_fn(page_fault)
            .set_stack_index(super::gdt::FAULT_IST_INDEX);
        idt.double_fault
            .set_handler_fn(double_fault)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        idt.non_maskable_interrupt.set_handler_fn(nmi); // §75 panic stop-CPU IPI

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

/// Load the (already-initialised) IDT on an Application Processor (§69 SMP Phase
/// 3). The IDT is shared — every CPU uses the same handler table; an AP only needs
/// to point its IDTR at it.
pub fn load_ap() {
    unsafe {
        let idt_ref: &'static InterruptDescriptorTable = &*addr_of!(IDT);
        idt_ref.load();
    }
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
    unsafe { super::lapic::eoi() };
    // §72: every CPU's LAPIC timer drives preemption, but TIMEKEEPING stays on the
    // BSP only — otherwise N cores would advance TICKS N× and fire the ~1 Hz tick
    // notif / timed-wait deadlines N× too fast. The BSP owns the monotonic clock;
    // APs just preempt.
    if crate::percpu::cpu_index() == 0 {
        let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        if tick % 100 == 0 {
            crate::notif::fire_tick();
        }
        crate::thread::wake_expired(tick);
    }
    let _ = frame;
    crate::thread::preempt();
}

extern "x86-interrupt" fn keyboard(_frame: InterruptStackFrame) {
    // §69 Phase 2c: IRQ1 arrives via the IOAPIC → LAPIC. EOI the LAPIC, then
    // signal the bound notif. We do NOT mask the line — see ioapic_irq! above:
    // the i8042 holds IRQ1 high while the output buffer is full, and masking an
    // edge-triggered line in that state loses the interrupt and wedges the shared
    // keyboard/mouse buffer. drain() empties the buffer, so no mask is needed.
    unsafe { super::lapic::eoi() };
    crate::irq::fire(1);
}

extern "x86-interrupt" fn serial_com1(_frame: InterruptStackFrame) {
    // IRQ4 — COM1 RX, also via the IOAPIC → LAPIC. EOI the LAPIC, signal. No mask,
    // for the same level-high-while-buffered reason as the i8042 (see above).
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
    let rip = frame.instruction_pointer.as_u64();
    // A ring-3 #UD is a buggy user program (e.g. a Rust `abort()`/`ud2`), not a
    // kernel fault — kill just that process, like the page-fault handler does, so a
    // bad userland instruction can't take down the whole machine.
    if frame.code_segment.0 & 3 == 3 {
        raw_str("[trap] #UD user rip=");
        raw_hex(rip);
        raw_str(" -- killing tcb ");
        raw_hex(crate::percpu::current() as u64);
        raw_putc(b'\n');
        crate::thread::kill_current_user();
    }
    panic!("#UD invalid opcode at rip={:#x}\n{:#?}", rip, frame);
}

// §77 DEBUG: dead-simple direct-to-UART output — touches ONLY the serial port and
// the passed-in values (no percpu, no locks, no fmt), so it can't itself fault.
fn raw_putc(b: u8) {
    use x86_64::instructions::port::Port;
    unsafe {
        let mut lsr = Port::<u8>::new(0x3FD);
        let mut s = 0u32;
        while lsr.read() & 0x20 == 0 {
            s += 1;
            if s > 1_000_000 {
                break;
            }
        }
        Port::<u8>::new(0x3F8).write(b);
    }
}
fn raw_str(s: &str) {
    for &b in s.as_bytes() {
        raw_putc(b);
    }
}
fn raw_hex(v: u64) {
    raw_str("0x");
    for i in (0..16).rev() {
        let n = ((v >> (i * 4)) & 0xf) as u8;
        raw_putc(if n < 10 { b'0' + n } else { b'a' + n - 10 });
    }
}

/// §77: a KERNEL fault is fatal — stop the other cores (NMI) and print the oops via
/// the lock-bypassing console, then halt. Runs on the IST fault stack (§77), so it
/// survives even a corrupted thread kernel stack (otherwise the handler re-faults on
/// the bad stack and the machine hangs silently).
fn fault_stop(name: &str, ring: u64, rip: u64, cr2: u64, err: u64, tid: usize) -> ! {
    if crate::PANICKED.swap(true, Ordering::SeqCst) {
        loop {
            unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
        }
    }
    unsafe { super::lapic::send_nmi_all_but_self() };
    for _ in 0..6_000_000 {
        core::hint::spin_loop();
    }
    raw_str("\n[FAULT] ");
    raw_str(name);
    raw_str(" ring=");
    raw_hex(ring);
    raw_str(" rip=");
    raw_hex(rip);
    raw_str(" cr2=");
    raw_hex(cr2);
    raw_str(" err=");
    raw_hex(err);
    raw_str(" tcb=");
    raw_hex(tid as u64);
    raw_putc(b'\n');
    for c in 0..8u64 {
        let r = crate::STOPPED_RIP[c as usize].load(Ordering::Acquire);
        if r != 0 {
            raw_str("  cpu ");
            raw_hex(c);
            raw_str(" at ");
            raw_hex(r);
            raw_putc(b'\n');
        }
    }
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

extern "x86-interrupt" fn general_protection_fault(frame: InterruptStackFrame, error_code: u64) {
    let tid = crate::percpu::current();
    // Ring-3 #GP kills the offending thread; the kernel lives on. Brief raw print
    // (no lock → can't deadlock under SMP), then kill.
    if frame.code_segment.0 & 3 == 3 {
        raw_str("[trap] #GP user rip=");
        raw_hex(frame.instruction_pointer.as_u64());
        raw_str(" err=");
        raw_hex(error_code);
        raw_str(" -- killing tcb ");
        raw_hex(tid as u64);
        raw_putc(b'\n');
        crate::thread::kill_current_user();
    }
    fault_stop("#GP", 0, frame.instruction_pointer.as_u64(), 0, error_code, tid);
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let cr2 = x86_64::registers::control::Cr2::read_raw();
    let tid = crate::percpu::current();
    if error_code.contains(PageFaultErrorCode::USER_MODE) {
        raw_str("[trap] #PF user cr2=");
        raw_hex(cr2);
        raw_str(" rip=");
        raw_hex(frame.instruction_pointer.as_u64());
        raw_str(" -- killing tcb ");
        raw_hex(tid as u64);
        raw_putc(b'\n');
        crate::thread::kill_current_user();
    }
    fault_stop("#PF", 0, frame.instruction_pointer.as_u64(), cr2, error_code.bits(), tid);
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    panic!("#DF error_code={:#x}\n{:#?}", error_code, frame);
}

/// §75: NMI handler. The panic path on another core broadcasts an NMI to stop every
/// other core (FreeBSD `stop_cpus_hard`). NMI is non-maskable, so we land here even
/// while spinning IF=0 or wedged in another handler. If a panic is in progress, halt
/// FOREVER (don't return — we may be mid-corruption and must touch nothing more). If
/// no panic is flagged, it's a spurious/hardware NMI: return and carry on.
extern "x86-interrupt" fn nmi(frame: InterruptStackFrame) {
    if crate::PANICKED.load(Ordering::Acquire) {
        // Record where we were wedged so the triggering core can print it, then halt.
        let cpu = crate::percpu::cpu_index();
        if cpu < 8 {
            crate::STOPPED_RIP[cpu].store(frame.instruction_pointer.as_u64(), Ordering::Release);
        }
        loop {
            unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
        }
    }
}
