//! hello — the simplest spawnable program: print one line via the tty, exit.
//!
//! It is launched by the shell with `run hello`. It holds no Console; its only
//! output channel is a tty R_SEND endpoint the shell granted at `SPAWN_STDOUT`
//! (slot 2), so it prints by sending a TAG_TTY_WRITE message — the same way the
//! shell does. Proves the spawn mechanism end to end.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, SPAWN_STDOUT, TAG_TTY_WRITE};
use oxbow_rt as rt;

/// Write a short (<63 byte) NUL-terminated string to the tty.
fn tw(s: &[u8]) {
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

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    tw(b"hello, world\n");
    rt::sys_exit(0);
}
