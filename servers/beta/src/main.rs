//! beta — the PONGER, in its own address space.
//!
//! Receives PING from pong (the pinger) over the boot endpoint and replies PONG,
//! crossing two address spaces with the kernel only mediating the rendezvous.
//! Serves two pings, then receives a third and exits WITHOUT replying — proving
//! the kernel abandons the orphaned reply and the blocked caller wakes E_GONE.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_CONSOLE, BOOT_EP, TAG_PONG, TAG_PING};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Receive one PING and reply PONG. If the PING carried a granted handle, use
/// it (write through the moved capability) before replying.
fn serve_once() {
    let mut m = MsgBuf::new(0);
    if let Ok(reply) = rt::sys_recv(BOOT_EP, &mut m) {
        if m.handle_count >= 1 {
            // A capability moved to us — write through its (our-table) index.
            let h = m.handles[0];
            let line = b"[P2] via granted console: a moved cap\n";
            let _ = rt::sys_console_write(h, line.as_ptr(), line.len());
        }
        if m.tag == TAG_PING {
            m.tag = TAG_PONG;
            m.data_len = 1;
            m.data[0] = u64::from_le_bytes(*b"PONG\n\0\0\0");
            m.handle_count = 0; // the reply carries no handles
            let _ = rt::sys_reply(reply, &m);
        }
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[P2] ponger ready\n");
    serve_once(); // round 1
    serve_once(); // round 2

    // Round 3: receive a ping but exit WITHOUT replying. The kernel must abandon
    // the orphaned Reply and wake the caller with E_GONE.
    let mut m = MsgBuf::new(0);
    let _ = rt::sys_recv(BOOT_EP, &mut m);
    w(b"[P2] got 3rd ping, exiting without reply\n");
    rt::sys_exit(0)
}
