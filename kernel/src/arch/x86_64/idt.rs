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
        idt[TIMER_VECTOR].set_handler_fn(timer);
        idt[0x27].set_handler_fn(spurious_master); // IRQ7 spurious: no EOI
        idt[0x2F].set_handler_fn(spurious_slave); // IRQ15 spurious: EOI master
        for v in 0x21u8..=0x2E {
            if v != 0x27 {
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
static USER_PREEMPTS: AtomicU64 = AtomicU64::new(0);

extern "x86-interrupt" fn timer(frame: InterruptStackFrame) {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // EOI BEFORE the (possible) context switch: if we switched first, the PIC's
    // in-service bit would block all further IRQ0s until this exact thread
    // resumes — a silent scheduler freeze. Safe here because IF=0 in the gate.
    pic::eoi(0);

    // Were we interrupting ring 3? (RPL bits of the saved CS.) Announce the
    // first few user preemptions as the Phase-5 proof that ring 3 is preemptible.
    if frame.code_segment.0 & 3 == 3 && USER_PREEMPTS.fetch_add(1, Ordering::Relaxed) < 3 {
        println!("[sched] preempt user @ tick {}", tick);
    }
    crate::thread::preempt();
}

extern "x86-interrupt" fn spurious_master(_frame: InterruptStackFrame) {
    // IRQ7 spurious interrupt — must NOT be EOI'd.
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
    panic!(
        "#GP error_code={:#x} at rip={:#x}\n{:#?}",
        error_code,
        frame.instruction_pointer.as_u64(),
        frame
    );
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let cr2 = x86_64::registers::control::Cr2::read();
    panic!(
        "#PF accessing {:?}, error={:?}, at rip={:#x}\n{:#?}",
        cr2,
        error_code,
        frame.instruction_pointer.as_u64(),
        frame
    );
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    panic!("#DF error_code={:#x}\n{:#?}", error_code, frame);
}
