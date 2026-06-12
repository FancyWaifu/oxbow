//! 16550 UART serial console on COM1. x86-specific (port-mapped I/O).
use core::fmt;
use spin::Mutex;
use uart_16550::SerialPort;

/// COM1. `SerialPort::new` is a `const unsafe fn`; 0x3F8 is the standard COM1
/// I/O base. Wrapped in a spinlock so prints from anywhere are serialized.
pub static SERIAL1: Mutex<SerialPort> = Mutex::new(unsafe { SerialPort::new(0x3F8) });

/// Initialize the UART (line/FIFO/IRQ setup). Call once, early.
pub fn init() {
    SERIAL1.lock().init();
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
