//! 16550 UART serial console on COM1. x86-specific (port-mapped I/O).
use core::fmt;
use spin::Mutex;
use uart_16550::SerialPort;

/// COM1. `SerialPort::new` is a `const unsafe fn`; 0x3F8 is the standard COM1
/// I/O base. Wrapped in a spinlock so prints from anywhere are serialized.
pub static SERIAL1: Mutex<SerialPort> = Mutex::new(unsafe { SerialPort::new(0x3F8) });

/// Initialize the UART (line/FIFO/IRQ setup). Call once, early.
///
/// The `uart_16550` crate's `init()` leaves the RX-data interrupt enabled
/// (IER=0x01) and OUT2 set (MCR=0x0b, gating the IRQ onto the PIC), so the
/// userspace serial RX driver needs to touch NO config registers — it only
/// READS RBR/LSR. We override one thing: the FIFO trigger level. The crate
/// sets it to 14 bytes (FCR=0xc7); we set 1 byte (FCR=0x07) so a single
/// keystroke raises IRQ4 deterministically instead of relying on the emulated
/// character-timeout. The kernel owns ALL UART config registers; the driver
/// must not (and cannot — its IoPort caps are R_IN only) write any of them.
pub fn init() {
    SERIAL1.lock().init();
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(0x3FA).write(0x07u8);
    }
}

/// Backing function for the `print!`/`println!` macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    // `.ok()` — there is nowhere useful to report a serial write error to.
    SERIAL1.lock().write_fmt(args).ok();
}

/// Write raw bytes to the console (backs the `sys_console_write` syscall).
pub fn write_bytes(bytes: &[u8]) {
    let mut port = SERIAL1.lock();
    for &b in bytes {
        port.send(b);
    }
}
