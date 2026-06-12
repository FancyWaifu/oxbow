//! Legacy 8259 PIC pair: remap to vectors 0x20-0x2F and selectively unmask.
//!
//! v0 simply masked both PICs (interrupts never fired). v1 remaps them off the
//! CPU exception range (0x00-0x1F) so hardware IRQs land on 0x20+, then unmasks
//! only the lines we want (IRQ0 = PIT timer). Pure port I/O — no MMIO, so no new
//! page-table machinery (the reason we use the PIC, not the LAPIC, this arc).
use x86_64::instructions::port::Port;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x11; // begin init, ICW4 will follow
const ICW4_8086: u8 = 0x01; // 8086/88 mode
const EOI: u8 = 0x20; // end-of-interrupt command

const OFFSET1: u8 = 0x20; // master IRQs 0-7  -> vectors 0x20-0x27
const OFFSET2: u8 = 0x28; // slave  IRQs 8-15 -> vectors 0x28-0x2F

/// Short delay between PIC writes (some hardware needs it). Writing to the
/// unused port 0x80 is the traditional ~1µs I/O wait.
fn io_wait() {
    unsafe {
        Port::<u8>::new(0x80).write(0);
    }
}

/// Reinitialize both PICs with vector offsets 0x20/0x28 and cascade on IRQ2.
pub fn remap() {
    unsafe {
        let mut c1 = Port::<u8>::new(PIC1_CMD);
        let mut d1 = Port::<u8>::new(PIC1_DATA);
        let mut c2 = Port::<u8>::new(PIC2_CMD);
        let mut d2 = Port::<u8>::new(PIC2_DATA);

        c1.write(ICW1_INIT);
        io_wait();
        c2.write(ICW1_INIT);
        io_wait();
        d1.write(OFFSET1); // ICW2: master offset
        io_wait();
        d2.write(OFFSET2); // ICW2: slave offset
        io_wait();
        d1.write(4); // ICW3: slave is on master IRQ2 (bit 2)
        io_wait();
        d2.write(2); // ICW3: slave cascade identity = 2
        io_wait();
        d1.write(ICW4_8086);
        io_wait();
        d2.write(ICW4_8086);
        io_wait();
    }
}

/// Mask (disable) every IRQ line on both PICs.
pub fn mask_all() {
    unsafe {
        Port::<u8>::new(PIC1_DATA).write(0xFF);
        Port::<u8>::new(PIC2_DATA).write(0xFF);
    }
}

/// Unmask (enable) one IRQ line. v1 arc 1 uses only IRQ0 (master).
pub fn unmask(irq: u8) {
    unsafe {
        let (port, line) = if irq < 8 {
            (PIC1_DATA, irq)
        } else {
            (PIC2_DATA, irq - 8)
        };
        let mut p = Port::<u8>::new(port);
        let mask = p.read() & !(1 << line);
        p.write(mask);
    }
}

/// Mask (disable) one IRQ line.
pub fn mask(irq: u8) {
    unsafe {
        let (port, line) = if irq < 8 {
            (PIC1_DATA, irq)
        } else {
            (PIC2_DATA, irq - 8)
        };
        let mut p = Port::<u8>::new(port);
        let val = p.read() | (1 << line);
        p.write(val);
    }
}

/// Acknowledge an IRQ. Slave-line IRQs (8-15) also EOI the master cascade.
pub fn eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            Port::<u8>::new(PIC2_CMD).write(EOI);
        }
        Port::<u8>::new(PIC1_CMD).write(EOI);
    }
}
