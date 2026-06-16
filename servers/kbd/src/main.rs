//! kbd — a user-mode keyboard driver (module 2), in its own address space.
//!
//! Interrupt-driven: it binds the keyboard IRQ to a notification and blocks on
//! it; when a key is pressed, the kernel's tiny handler signals the notification,
//! the driver wakes, drains the i8042 (its own I/O-port caps), and acks to re-arm
//! the line. The kernel never touches the keyboard — this unprivileged process
//! holds all the hardware authority as capabilities.
//!
//! It implements a real US-QWERTY layout: Shift and Caps Lock (uppercase +
//! shifted symbols) and Left-Ctrl (Ctrl+letter -> a control byte, so Ctrl-C/U/W
//! reach the tty line discipline). Modifier state persists across interrupts.
#![no_std]
#![no_main]

use oxbow_abi::{
    MsgBuf, SysError, BOOT_CONSOLE, BOOT_INPUT_CHAN, BOOT_IRQ, BOOT_KBD_DATA, BOOT_KBD_STATUS,
    BOOT_MOUSE_CHAN, BOOT_MOUSE_IRQ, BOOT_TTY, R_IN, TAG_TTY_CHAR,
};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Held-modifier state, persistent across IRQs (one drain per interrupt).
struct Mods {
    shift: bool,
    ctrl: bool,
    caps: bool,
}

/// PS/2 mouse packet assembly (§54): the device sends 3-byte packets. We forward
/// each completed packet verbatim to the compositor, which decodes it.
struct Mouse {
    pkt: [u8; 3],
    idx: usize,
}

// --- i8042 controller I/O (the kbd driver owns 0x60/0x64) -------------------
fn i8042_status() -> u8 {
    rt::sys_io_in(BOOT_KBD_STATUS, 0x64).unwrap_or(0)
}
/// Wait until the input buffer is empty (status bit 1 clear) so a write lands.
fn wait_write() {
    for _ in 0..200_000 {
        if i8042_status() & 0x02 == 0 {
            return;
        }
    }
}
/// Wait until the output buffer is full (status bit 0 set) so a read is valid.
fn wait_read() {
    for _ in 0..200_000 {
        if i8042_status() & 0x01 != 0 {
            return;
        }
    }
}
fn ctrl_cmd(c: u8) {
    wait_write();
    let _ = rt::sys_io_out(BOOT_KBD_STATUS, 0x64, c);
}
fn data_write(d: u8) {
    wait_write();
    let _ = rt::sys_io_out(BOOT_KBD_DATA, 0x60, d);
}
fn data_read() -> u8 {
    wait_read();
    rt::sys_io_in(BOOT_KBD_DATA, 0x60).unwrap_or(0)
}
/// Send a command byte to the mouse (aux device) — prefixed with 0xD4.
fn mouse_cmd(d: u8) {
    ctrl_cmd(0xD4);
    data_write(d);
}
/// Initialise the PS/2 mouse: enable the aux port + IRQ12, then data reporting.
fn mouse_init() {
    ctrl_cmd(0xA8); // enable the auxiliary (mouse) device
    ctrl_cmd(0x20); // "read controller config byte"
    let mut cfg = data_read();
    cfg |= 0x02; // enable IRQ12 (aux interrupt)
    cfg &= !0x20; // clear "aux clock disable" → enable the mouse clock
    ctrl_cmd(0x60); // "write controller config byte"
    data_write(cfg);
    mouse_cmd(0xF6); // set defaults
    let _ = data_read(); // ACK (0xFA)
    mouse_cmd(0xF4); // enable data reporting
    let _ = data_read(); // ACK (0xFA)
}

/// Scancode set 1 make-code → (unshifted, shifted) ASCII for the US-QWERTY main
/// block. (0, 0) = a key we don't translate to a character (modifiers, F-keys,
/// keypad, etc.). QEMU's i8042 does XT translation by default, so we see set 1.
fn keychar(sc: u8) -> (u8, u8) {
    match sc {
        0x02 => (b'1', b'!'),
        0x03 => (b'2', b'@'),
        0x04 => (b'3', b'#'),
        0x05 => (b'4', b'$'),
        0x06 => (b'5', b'%'),
        0x07 => (b'6', b'^'),
        0x08 => (b'7', b'&'),
        0x09 => (b'8', b'*'),
        0x0A => (b'9', b'('),
        0x0B => (b'0', b')'),
        0x0C => (b'-', b'_'),
        0x0D => (b'=', b'+'),
        0x0E => (0x08, 0x08), // backspace
        0x0F => (b'\t', b'\t'),
        0x10 => (b'q', b'Q'),
        0x11 => (b'w', b'W'),
        0x12 => (b'e', b'E'),
        0x13 => (b'r', b'R'),
        0x14 => (b't', b'T'),
        0x15 => (b'y', b'Y'),
        0x16 => (b'u', b'U'),
        0x17 => (b'i', b'I'),
        0x18 => (b'o', b'O'),
        0x19 => (b'p', b'P'),
        0x1A => (b'[', b'{'),
        0x1B => (b']', b'}'),
        0x1C => (b'\n', b'\n'), // enter
        0x1E => (b'a', b'A'),
        0x1F => (b's', b'S'),
        0x20 => (b'd', b'D'),
        0x21 => (b'f', b'F'),
        0x22 => (b'g', b'G'),
        0x23 => (b'h', b'H'),
        0x24 => (b'j', b'J'),
        0x25 => (b'k', b'K'),
        0x26 => (b'l', b'L'),
        0x27 => (b';', b':'),
        0x28 => (b'\'', b'"'),
        0x29 => (b'`', b'~'),
        0x2B => (b'\\', b'|'),
        0x2C => (b'z', b'Z'),
        0x2D => (b'x', b'X'),
        0x2E => (b'c', b'C'),
        0x2F => (b'v', b'V'),
        0x30 => (b'b', b'B'),
        0x31 => (b'n', b'N'),
        0x32 => (b'm', b'M'),
        0x33 => (b',', b'<'),
        0x34 => (b'.', b'>'),
        0x35 => (b'/', b'?'),
        0x37 => (b'*', b'*'), // keypad *
        0x39 => (b' ', b' '),
        _ => (0, 0),
    }
}

/// Drain every byte the i8042 has buffered (status 0x64 bit 0 = output full),
/// updating modifier state and forwarding translated characters to the tty.
/// 0xE0-extended keys (arrows, right-side modifiers) are swallowed for now.
fn mouse_byte(m: &mut Mouse, b: u8) {
    // The first packet byte always has bit 3 set — use it to resync if we slip.
    if m.idx == 0 && b & 0x08 == 0 {
        return;
    }
    m.pkt[m.idx] = b;
    m.idx += 1;
    if m.idx == 3 {
        m.idx = 0;
        rt::channel::send(BOOT_MOUSE_CHAN, &m.pkt, &[]);
    }
}

fn drain(mods: &mut Mods, mouse: &mut Mouse) {
    let mut ext = false;
    loop {
        let status = i8042_status();
        if status & 0x01 == 0 {
            break; // output buffer empty — nothing more to drain
        }
        let sc = match rt::sys_io_in(BOOT_KBD_DATA, 0x60) {
            Ok(v) => v,
            Err(_) => break,
        };
        // §54: status bit 5 set ⟹ this byte came from the mouse (aux device).
        if status & 0x20 != 0 {
            mouse_byte(mouse, sc);
            continue;
        }
        if sc == 0xE0 {
            ext = true; // extended-key prefix — swallow the next code
            continue;
        }
        if ext {
            ext = false;
            continue;
        }
        // §48: forward the raw set-1 scancode (make AND break) to the compositor
        // for xkb decoding. Main-block set-1 make codes == evdev keycodes, so the
        // compositor just masks the break bit. The ASCII→tty path below is kept so
        // the serial console still works.
        rt::channel::send(BOOT_INPUT_CHAN, &[sc], &[]);
        match sc {
            0x2A | 0x36 => mods.shift = true,  // L/R Shift make
            0xAA | 0xB6 => mods.shift = false, // L/R Shift break
            0x1D => mods.ctrl = true,          // L Ctrl make
            0x9D => mods.ctrl = false,         // L Ctrl break
            0x3A => mods.caps = !mods.caps,    // Caps Lock toggles on make
            _ if sc & 0x80 != 0 => {}          // any other key release: ignore
            _ => {
                let (base, shifted) = keychar(sc);
                if base == 0 {
                    continue;
                }
                let mut c = if mods.shift { shifted } else { base };
                // Caps Lock inverts letter case only.
                if mods.caps && base.is_ascii_lowercase() {
                    c = if mods.shift { base } else { shifted };
                }
                // Ctrl+letter → the corresponding control byte (Ctrl-C = 0x03,
                // Ctrl-U = 0x15, Ctrl-W = 0x17, ...), which the tty acts on.
                if mods.ctrl && c.is_ascii_alphabetic() {
                    c &= 0x1F;
                }
                let mut m = MsgBuf::new(TAG_TTY_CHAR);
                m.data_len = 1;
                m.data[0] = c as u64;
                let _ = rt::sys_send(BOOT_TTY, &m);
            }
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

    let mut mods = Mods { shift: false, ctrl: false, caps: false };
    let mut mouse = Mouse { pkt: [0; 3], idx: 0 };
    let _ = rt::sys_irq_bind(BOOT_IRQ, notif);
    // §54: initialise the PS/2 mouse, then bind its IRQ (12) to the SAME notif —
    // both keyboard and mouse interrupts wake us; drain() routes by status bit 5.
    mouse_init();
    let _ = rt::sys_irq_bind(BOOT_MOUSE_IRQ, notif);
    w(b"[kbd] ps/2 mouse enabled (irq12)\n");
    // Drain anything buffered from boot, then ack both lines to arm them.
    drain(&mut mods, &mut mouse);
    let _ = rt::sys_irq_ack(BOOT_IRQ);
    let _ = rt::sys_irq_ack(BOOT_MOUSE_IRQ);

    loop {
        let _ = rt::sys_notif_wait(notif); // block until a kbd OR mouse IRQ fires
        drain(&mut mods, &mut mouse); // read all buffered bytes before acking
        let _ = rt::sys_irq_ack(BOOT_IRQ); // re-arm both lines
        let _ = rt::sys_irq_ack(BOOT_MOUSE_IRQ);
    }
}
