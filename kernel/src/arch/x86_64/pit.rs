//! 8254 PIT, channel 0 — the v1 scheduler tick source.
//!
//! Channel 0's output is wired to PIC IRQ0. Mode 3 (square wave) with a divisor
//! of the 1.193182 MHz base clock gives a periodic interrupt. Pure port I/O.
use x86_64::instructions::port::Port;

const CHANNEL0: u16 = 0x40;
const COMMAND: u16 = 0x43;
const BASE_HZ: u32 = 1_193_182;

/// Program channel 0 for a periodic `freq_hz` interrupt (clamped to the divisor
/// range). 100 Hz → divisor 11931 → ~10 ms ticks.
pub fn init(freq_hz: u32) {
    let divisor = (BASE_HZ / freq_hz).clamp(1, 0xFFFF) as u16;
    unsafe {
        // 0x36 = channel 0, access lobyte/hibyte, mode 3 (square wave), binary.
        Port::<u8>::new(COMMAND).write(0x36);
        let mut ch0 = Port::<u8>::new(CHANNEL0);
        ch0.write((divisor & 0xFF) as u8);
        ch0.write((divisor >> 8) as u8);
    }
}
