//! IRQ routing: bind a hardware line to a user driver's Notification.
//!
//! Discipline (one rule for every line): the handler MASKS the line and EOIs in
//! the kernel at fire time, then signals the bound notification. The driver
//! drains the device and `ack`s (unmask) when ready for the next interrupt.
//! EOI-in-kernel is mandatory — an in-service ISR bit held across a context
//! switch blocks all equal/lower lines machine-wide. Mask-on-fire means a
//! never-acking driver can't storm the CPU.
use spin::Mutex;

/// Line number → bound notification pool index.
static BINDINGS: Mutex<[Option<u8>; 16]> = Mutex::new([None; 16]);

/// Bind IRQ `line` to notification `notif_idx`. Does NOT unmask — the first
/// `ack` arms the line.
pub fn bind(line: u8, notif_idx: u8) {
    BINDINGS.lock()[line as usize] = Some(notif_idx);
}

/// Ack: re-arm the line (unmask) for the next interrupt. The driver calls this
/// AFTER draining the device. §69 Phase 2c: lines routed through the IOAPIC
/// (keyboard/mouse/serial) are re-armed there; PCI lines stay on the PIC.
pub fn ack(line: u8) {
    if crate::arch::ioapic::routed(line) {
        crate::arch::ioapic::unmask(line);
    } else {
        crate::arch::pic_unmask(line);
    }
}

/// True if the line has a binding (so `ack` is meaningful).
pub fn is_bound(line: u8) -> bool {
    BINDINGS.lock()[line as usize].is_some()
}

/// Called from the IRQ handler (IF=0): signal the bound notification (wake-only,
/// no block, no switch). The handler has already masked + EOI'd the line.
pub fn fire(line: u8) {
    // Copy the binding OUT and drop BINDINGS before signalling (§73 lock-order
    // hygiene): `signal` takes notif POOL, so holding BINDINGS across it would nest
    // BINDINGS > POOL. It's acyclic, but there's no reason to widen the scope — the
    // binding is a single byte.
    let bound = BINDINGS.lock()[line as usize];
    if let Some(idx) = bound {
        crate::notif::signal(idx);
    }
}
