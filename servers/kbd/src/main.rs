//! kbd — a user-mode keyboard driver (module 2), in its own address space.
//!
//! Interrupt-driven: it binds the keyboard IRQ to a notification and blocks on
//! it; when a key is pressed, the kernel's tiny handler signals the notification,
//! the driver wakes, drains the i8042 (its own I/O-port caps), and acks to re-arm
//! the line. The kernel never touches the keyboard — this unprivileged process
//! holds all the hardware authority as capabilities.
#![no_std]
#![no_main]

use oxbow_abi::{SysError, BOOT_CONSOLE, BOOT_IRQ, BOOT_KBD_DATA, BOOT_KBD_STATUS, R_IN};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Scancode set 1 make-code → ASCII (letters, digits, space, enter). 0 = none.
/// QEMU's i8042 does XT translation by default, so we see set 1.
fn ascii(sc: u8) -> u8 {
    match sc {
        0x1E => b'a', 0x30 => b'b', 0x2E => b'c', 0x20 => b'd', 0x12 => b'e',
        0x21 => b'f', 0x22 => b'g', 0x23 => b'h', 0x17 => b'i', 0x24 => b'j',
        0x25 => b'k', 0x26 => b'l', 0x32 => b'm', 0x31 => b'n', 0x18 => b'o',
        0x19 => b'p', 0x10 => b'q', 0x13 => b'r', 0x1F => b's', 0x14 => b't',
        0x16 => b'u', 0x2F => b'v', 0x11 => b'w', 0x2D => b'x', 0x15 => b'y',
        0x2C => b'z', 0x02 => b'1', 0x03 => b'2', 0x04 => b'3', 0x05 => b'4',
        0x06 => b'5', 0x07 => b'6', 0x08 => b'7', 0x09 => b'8', 0x0A => b'9',
        0x0B => b'0', 0x39 => b' ', 0x1C => b'\n',
        _ => 0,
    }
}

/// Drain every byte the i8042 has buffered (status 0x64 bit 0 = output full),
/// translating make codes to characters. Break codes (bit 7) and 0xE0-extended
/// keys are ignored.
fn drain() {
    let mut ext = false;
    while rt::sys_io_in(BOOT_KBD_STATUS, 0x64).map(|s| s & 1 != 0).unwrap_or(false) {
        let sc = match rt::sys_io_in(BOOT_KBD_DATA, 0x60) {
            Ok(v) => v,
            Err(_) => break,
        };
        if sc == 0xE0 {
            ext = true; // extended-key prefix — swallow the next code
            continue;
        }
        if ext {
            ext = false;
            continue;
        }
        if sc & 0x80 != 0 {
            continue; // key release
        }
        let c = ascii(sc);
        if c != 0 {
            w(b"[kbd] you typed: ");
            w(&[c]);
            w(b"\n");
        }
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[kbd] ready (irq-driven)\n");

    let notif = rt::sys_notif_create().expect("kbd: notif");

    // Capability-enforcement checks on the new syscalls.
    match rt::sys_irq_bind(BOOT_CONSOLE, notif) {
        Err(SysError::BadType) => w(b"[kbd] bind non-irq denied (E_BAD_TYPE) ok\n"),
        _ => w(b"[kbd] bind non-irq NOT denied\n"),
    }
    match rt::sys_io_in(BOOT_KBD_DATA, 0x64) {
        Err(SysError::Msg) => w(b"[kbd] out-of-range port denied ok\n"),
        _ => w(b"[kbd] out-of-range NOT denied\n"),
    }
    if let Ok(ro) = rt::sys_attenuate(BOOT_KBD_DATA, R_IN) {
        match rt::sys_io_out(ro, 0x60, 0) {
            Err(SysError::Rights) => w(b"[kbd] io_out without R_OUT denied (E_RIGHTS) ok\n"),
            _ => w(b"[kbd] io_out without R_OUT NOT denied\n"),
        }
    }

    let _ = rt::sys_irq_bind(BOOT_IRQ, notif);
    // Drain anything buffered from boot, then ack to arm the line.
    drain();
    let _ = rt::sys_irq_ack(BOOT_IRQ);

    loop {
        let _ = rt::sys_notif_wait(notif); // block until the keyboard IRQ fires
        drain(); // read the scancode(s) — MUST drain before acking (edge line)
        let _ = rt::sys_irq_ack(BOOT_IRQ); // re-arm for the next interrupt
    }
}
