//! tty — the line-discipline server (module 3), sole receiver on the TTY
//! endpoint. It owns the Console. Three message kinds arrive on one endpoint,
//! distinguished by tag:
//!   TAG_TTY_CHAR  (kbd, one-way)  — a keystroke; run line discipline.
//!   TAG_TTY_READ  (shell, call)   — read a line; reply when one is ready.
//!   TAG_TTY_WRITE (shell, one-way)— output text; write it to the console.
//!
//! A single recv loop multiplexes them. A shell READ that arrives before a line
//! is ready has its Reply handle STASHED; the next completed line is replied to
//! it. Completed lines arriving while no shell waits queue in a small FIFO.
#![no_std]
#![no_main]

use oxbow_abi::{
    MsgBuf, BOOT_CONSOLE, BOOT_TTY, HANDLE_NULL, TAG_TTY_CHAR, TAG_TTY_LINE, TAG_TTY_READ,
    TAG_TTY_WRITE,
};
use oxbow_rt as rt;

const MAX_LINE: usize = 63;
const DONE_CAP: usize = 4;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Pack a line (NUL-terminated) into a TAG_TTY_LINE message.
fn pack_line(line: &[u8]) -> MsgBuf {
    let mut m = MsgBuf::new(TAG_TTY_LINE);
    let n = core::cmp::min(line.len(), MAX_LINE);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(line.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    m
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[tty] ready\n");

    let mut edit = [0u8; 64];
    let mut elen = 0usize;
    let mut done = [[0u8; 64]; DONE_CAP];
    let mut dlen = [0usize; DONE_CAP];
    let mut dhead = 0usize;
    let mut dcount = 0usize;
    let mut pending = HANDLE_NULL; // stashed shell Reply handle, or HANDLE_NULL

    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_TTY, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match m.tag {
            TAG_TTY_CHAR if m.data_len >= 1 => match m.data[0] as u8 {
                0x08 => {
                    if elen > 0 {
                        elen -= 1;
                        w(b"\x08 \x08");
                    }
                }
                b'\n' | b'\r' => {
                    w(b"\n");
                    // Push the completed line into the done FIFO.
                    if dcount < DONE_CAP {
                        let slot = (dhead + dcount) % DONE_CAP;
                        done[slot][..elen].copy_from_slice(&edit[..elen]);
                        dlen[slot] = elen;
                        dcount += 1;
                    } else {
                        w(b"[tty] !line dropped\n");
                    }
                    elen = 0;
                    // Deliver to a waiting shell, if any.
                    if pending != HANDLE_NULL && dcount > 0 {
                        let slot = dhead;
                        dhead = (dhead + 1) % DONE_CAP;
                        dcount -= 1;
                        let rm = pack_line(&done[slot][..dlen[slot]]);
                        let _ = rt::sys_reply(pending, &rm);
                        pending = HANDLE_NULL;
                    }
                }
                c @ 0x20..=0x7E => {
                    if elen < MAX_LINE {
                        edit[elen] = c;
                        elen += 1;
                        w(&[c]); // echo as typed
                    }
                }
                _ => {}
            },
            TAG_TTY_READ => {
                if dcount > 0 {
                    let slot = dhead;
                    dhead = (dhead + 1) % DONE_CAP;
                    dcount -= 1;
                    let rm = pack_line(&done[slot][..dlen[slot]]);
                    let _ = rt::sys_reply(reply, &rm);
                } else if pending != HANDLE_NULL {
                    // Only one shell expected; reply an empty line defensively.
                    let _ = rt::sys_reply(reply, &pack_line(b""));
                } else {
                    pending = reply; // stash until a line completes
                }
            }
            TAG_TTY_WRITE => {
                // Shell output: write the NUL-terminated payload to the console.
                let bytes =
                    unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let n = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                w(&bytes[..n]);
            }
            _ => {}
        }
    }
}
