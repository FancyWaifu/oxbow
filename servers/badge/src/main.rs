//! badge — a spawnable demo server for §14 badged endpoints.
//!
//! It receives three messages on its endpoint (slot 1 = BOOT_EP) and reports the
//! badge the kernel delivered for each — `[badge] got N`. The shell sends those
//! three via two badged caps (7, 42) and one unbadged cap into which it tried to
//! forge a badge; the kernel-stamped values prove delivery + unforgeability.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_EP, SPAWN_STDOUT, TAG_TTY_WRITE};
use oxbow_rt as rt;

/// Write a short (<63 byte) line to stdout (a granted tty endpoint at slot 2).
fn w(s: &[u8]) {
    let mut m = MsgBuf::new(TAG_TTY_WRITE);
    let n = core::cmp::min(s.len(), 63);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(s.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    let _ = rt::sys_send(SPAWN_STDOUT, &m);
}

/// Print a u64 as decimal (badges are small here).
fn w_u64(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        w(b"0");
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    w(&buf[i..]);
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    for _ in 0..3 {
        let mut m = MsgBuf::new(0);
        if rt::sys_recv(BOOT_EP, &mut m).is_ok() {
            w(b"[badge] got ");
            w_u64(m.badge);
            w(b"\n");
        }
    }
    w(b"[badge] done\n");
    rt::sys_exit(0)
}
