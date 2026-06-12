//! beta — the PONGER, in its own address space.
//!
//! Receives PING from pong (the pinger) and replies PONG across two address
//! spaces. Round 1's PING also carries a read-only Frame capability: beta maps
//! it and reads the shared message zero-copy, and proves it cannot map it
//! writable (the capability was attenuated to read-only). Then it serves a
//! second ping and exits without replying a third — proving E_GONE.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, SysError, BOOT_CONSOLE, BOOT_EP, PROT_READ, PROT_WRITE, TAG_PONG, TAG_PING};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// If a Frame capability rode along, map it read-only and read the shared
/// message, then prove a writable mapping is denied.
fn handle_shared_frame(frame: u32) {
    const SHARED: u64 = 0x5000_0000;
    if rt::sys_frame_map(frame, SHARED, PROT_READ).is_ok() {
        let mut len = 0u64;
        while len < 31 && unsafe { core::ptr::read_volatile((SHARED + len) as *const u8) } != 0 {
            len += 1;
        }
        w(b"[P2] shmem: ");
        let _ = rt::sys_console_write(BOOT_CONSOLE, SHARED as *const u8, len as usize);
        w(b" (zero copy)\n");
    }
    // The handle is read-only (attenuated) — a writable map must be refused.
    match rt::sys_frame_map(frame, 0x5001_0000, PROT_READ | PROT_WRITE) {
        Err(SysError::Rights) => w(b"[P2] ro frame: writable map denied (E_RIGHTS) ok\n"),
        _ => w(b"[P2] ro frame: writable map NOT denied!\n"),
    }
}

/// Receive one PING and reply PONG (handling a shared frame if present).
fn serve_once() {
    let mut m = MsgBuf::new(0);
    if let Ok(reply) = rt::sys_recv(BOOT_EP, &mut m) {
        if m.handle_count >= 1 {
            handle_shared_frame(m.handles[0]);
        }
        if m.tag == TAG_PING {
            m.tag = TAG_PONG;
            m.data_len = 1;
            m.data[0] = u64::from_le_bytes(*b"PONG\n\0\0\0");
            m.handle_count = 0;
            let _ = rt::sys_reply(reply, &m);
        }
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[P2] ponger ready\n");
    serve_once(); // round 1 (with the shared frame)
    serve_once(); // round 2

    // Round 3: receive a ping but exit WITHOUT replying -> caller gets E_GONE.
    let mut m = MsgBuf::new(0);
    let _ = rt::sys_recv(BOOT_EP, &mut m);
    w(b"[P2] got 3rd ping, exiting without reply\n");
    rt::sys_exit(0)
}
