//! beta — the second oxbow user-mode server, in its own address space.
//!
//! Linked at the SAME vaddr 0x200000 as pong, but isolated by a separate PML4.
//! Prints a greeting, then bursts of `b ` markers (interleaving with pong's
//! `u ` under preemption). With `--features isolation` it also prints its own
//! bytes at 0x200000 (different from pong's) and then touches an unmapped
//! address — proving it dies alone while pong keeps running.
#![no_std]
#![no_main]

use oxbow_abi::BOOT_CONSOLE;
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Print the low byte of `v` as two hex digits.
#[cfg(feature = "isolation")]
fn hex_byte(v: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    w(&[HEX[(v >> 4) as usize], HEX[(v & 0xf) as usize]]);
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[P2] hello from the beta world\n");

    #[cfg(feature = "isolation")]
    {
        // Read our OWN 8 bytes at 0x200000 (legal: our text page is R+X+U).
        w(b"[P2] @0x200000 = ");
        for i in 0..8 {
            let b = unsafe { core::ptr::read_volatile((0x200000 + i) as *const u8) };
            hex_byte(b);
        }
        w(b"\n");
    }

    for _ in 0..4 {
        w(b"b ");
        let mut x: u64 = 0;
        for i in 0..3_000_000u64 {
            x = x.wrapping_add(i);
        }
        core::hint::black_box(x);
    }
    w(b"\n");

    // Touch an unmapped address — we should be killed; pong keeps running.
    #[cfg(feature = "isolation")]
    unsafe {
        core::ptr::read_volatile(0x0000_dead_0000u64 as *const u64);
    }

    rt::sys_exit(0)
}
