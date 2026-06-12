//! IDT — CPU exception handlers only.
//!
//! v0 is synchronous with no timer, PIC, or APIC (D5): the only things that can
//! transfer control to the kernel asynchronously are CPU exceptions, so those
//! are all we install. Every fault dumps its frame and panics — except #BP,
//! which prints and resumes — turning bugs into readable serial output instead
//! of triple-fault reboots.
use core::ptr::{addr_of, addr_of_mut};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::println;

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

/// Install the IDT and mask the legacy PICs (so a firmware-armed IRQ line can't
/// surprise us — we never enable interrupts in v0, but belt and suspenders).
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

        let idt_ref: &'static InterruptDescriptorTable = &*addr_of!(IDT);
        idt_ref.load();
    }
    mask_pics();
}

fn mask_pics() {
    use x86_64::instructions::port::Port;
    unsafe {
        let mut pic1_data: Port<u8> = Port::new(0x21);
        let mut pic2_data: Port<u8> = Port::new(0xA1);
        pic1_data.write(0xFF);
        pic2_data.write(0xFF);
    }
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
