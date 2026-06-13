//! serial — a user-mode 16550 COM1 RX driver (module 5), in its own address
//! space. It makes the serial line a real input device so you can type at the
//! shell directly over `-serial` (BSD serial-console style), in addition to the
//! PS/2 keyboard.
//!
//! Same interrupt-driven pattern as `kbd`: bind IRQ4 to a notification, block on
//! it, and on wake drain the UART receive FIFO and forward each byte to the tty.
//! The device is SHARED with the kernel (which owns config + the TX path); this
//! driver is granted RBR/LSR as R_IN-only I/O-port caps, so it can only READ —
//! a write to any UART register would fault E_RIGHTS. It touches no config.
//!
//! Each received byte is forwarded verbatim to the tty as a TAG_TTY_CHAR
//! message — the same one-way protocol the kbd driver uses — so serial input
//! joins keyboard input in the one line discipline. The driver does NO
//! translation (CR, DEL, etc. are line-discipline policy, handled by the tty).
#![no_std]
#![no_main]

use oxbow_abi::{
    MsgBuf, SysError, BOOT_CONSOLE, BOOT_SERIAL_IRQ, BOOT_SERIAL_LSR, BOOT_SERIAL_RBR, BOOT_TTY,
    TAG_TTY_CHAR,
};
use oxbow_rt as rt;

const RBR: u16 = 0x3F8; // receive buffer (read side of the data port)
const LSR: u16 = 0x3FD; // line status register; bit 0 = data ready

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Drain every byte the UART has buffered: while LSR bit0 (data ready) is set,
/// read RBR and forward it to the tty. With only the RX-data interrupt enabled,
/// draining below the FIFO trigger deasserts the IRQ — no IIR read needed.
fn drain() {
    while rt::sys_io_in(BOOT_SERIAL_LSR, LSR).map(|s| s & 1 != 0).unwrap_or(false) {
        let b = match rt::sys_io_in(BOOT_SERIAL_RBR, RBR) {
            Ok(v) => v,
            Err(_) => break,
        };
        let mut m = MsgBuf::new(TAG_TTY_CHAR);
        m.data_len = 1;
        m.data[0] = b as u64;
        let _ = rt::sys_send(BOOT_TTY, &m);
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[serial] ready (irq-driven)\n");

    let notif = rt::sys_notif_create().expect("serial: notif");

    // Capability-enforcement check: our RBR cap is R_IN only (no R_OUT), so a
    // write to the UART must be denied — proving the kernel keeps exclusive
    // ownership of the config/TX side. (The cap also lacks R_ATTENUATE by design,
    // so we test the original handle directly.)
    match rt::sys_io_out(BOOT_SERIAL_RBR, RBR, 0) {
        Err(SysError::Rights) => w(b"[serial] io_out on R_IN port denied (E_RIGHTS) ok\n"),
        _ => w(b"[serial] io_out NOT denied\n"),
    }

    let _ = rt::sys_irq_bind(BOOT_SERIAL_IRQ, notif);
    // Drain anything buffered from boot, then ack to arm the line.
    drain();
    let _ = rt::sys_irq_ack(BOOT_SERIAL_IRQ);

    loop {
        let _ = rt::sys_notif_wait(notif); // block until COM1 RX IRQ fires
        drain(); // read the byte(s) — MUST drain before acking (edge line)
        let _ = rt::sys_irq_ack(BOOT_SERIAL_IRQ); // re-arm for the next interrupt
    }
}
