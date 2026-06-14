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
//!
//! Echo is COOKED-MODE synchronized (§12.5): keystrokes echo live while a reader
//! waits, but buffer un-echoed while the shell is busy and flush at the next READ
//! (after the prompt) — so paste / type-ahead never tangles echo with output.
#![no_std]
#![no_main]

use oxbow_abi::{
    Handle, MsgBuf, BOOT_CONSOLE, BOOT_TTY, HANDLE_NULL, TAG_TTY_CHAR, TAG_TTY_LINE, TAG_TTY_READ,
    TAG_TTY_WRITE,
};
use oxbow_rt as rt;

/// Max line length. A line longer than one MsgBuf is delivered in chunks, so the
/// only cap is the buffer size — generous now (long `cc`/`curl` command lines).
const MAX_LINE: usize = 255;
const LINE_BUF: usize = 256;
const DONE_CAP: usize = 4;
/// Payload bytes per TAG_TTY_LINE chunk (data[2..8] = 6 u64 words).
const CHUNK: usize = 48;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Reply one chunk of `line` starting at `off`: data[0] = chunk byte count,
/// data[1] = 1 if more chunks follow (else 0), payload at byte offset 16. The
/// shell loops READ, accumulating chunks until `more` is 0. Returns the new off.
fn send_chunk(reply: Handle, line: &[u8], off: usize) -> usize {
    let n = core::cmp::min(CHUNK, line.len() - off);
    let mut m = MsgBuf::new(TAG_TTY_LINE);
    m.data[0] = n as u64;
    m.data[1] = if off + n < line.len() { 1 } else { 0 };
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(line[off..off + n].as_ptr(), dst.add(16), n);
    }
    m.data_len = 8;
    let _ = rt::sys_reply(reply, &m);
    off + n
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[tty] ready\n");

    let mut edit = [0u8; LINE_BUF];
    let mut elen = 0usize;
    // Cooked-mode echo cursor: edit[..echoed] is on screen, edit[echoed..elen] is
    // pending-echo. Echo is LIVE while a reader waits (pending set) and DEFERRED
    // while the shell is busy, then flushed on the next READ — so keystrokes can't
    // tangle with the shell's output/prompt under paste/type-ahead. Invariants:
    //  (1) pending set  ⟹ echoed == elen (the tail is always flushed when set);
    //  (2) pending null ⟹ new chars buffer un-echoed (the busy window);
    //  (3) a line resting in the done FIFO is always un-echoed (it completed with
    //      no reader), so it is echoed at the moment a READ pops it.
    let mut echoed = 0usize;
    let mut done = [[0u8; LINE_BUF]; DONE_CAP];
    let mut dlen = [0usize; DONE_CAP];
    let mut dhead = 0usize;
    let mut dcount = 0usize;
    let mut pending = HANDLE_NULL; // stashed shell Reply handle, or HANDLE_NULL
    // Chunked line delivery: `deliver[..dvlen]` is the line being streamed to the
    // shell, `dvoff` how much has gone out. `dvoff < dvlen` ⟹ mid-delivery.
    let mut deliver = [0u8; LINE_BUF];
    let mut dvlen = 0usize;
    let mut dvoff = 0usize;

    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_TTY, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match m.tag {
            TAG_TTY_CHAR if m.data_len >= 1 => match m.data[0] as u8 {
                // PS/2 backspace is 0x08; serial terminals send DEL (0x7F).
                0x08 | 0x7F => {
                    if elen > 0 {
                        elen -= 1;
                        if echoed > elen {
                            echoed = elen;
                            w(b"\x08 \x08"); // it was on screen — rub it out
                        }
                        // else: an un-echoed type-ahead char — vanish silently
                    }
                }
                // Ctrl-U: kill the whole input line. Rub out the echoed prefix;
                // un-echoed type-ahead just vanishes.
                0x15 => {
                    while elen > 0 {
                        elen -= 1;
                        if echoed > elen {
                            echoed = elen;
                            w(b"\x08 \x08");
                        }
                    }
                }
                // Ctrl-W: erase the last word (trailing spaces, then non-spaces).
                0x17 => {
                    let erase_one = |elen: &mut usize, echoed: &mut usize| {
                        *elen -= 1;
                        if *echoed > *elen {
                            *echoed = *elen;
                            w(b"\x08 \x08");
                        }
                    };
                    while elen > 0 && edit[elen - 1] == b' ' {
                        erase_one(&mut elen, &mut echoed);
                    }
                    while elen > 0 && edit[elen - 1] != b' ' {
                        erase_one(&mut elen, &mut echoed);
                    }
                }
                // Ctrl-C: cancel the current line. Echo ^C, drop the buffer, and if
                // a reader waits, hand it an empty line so the shell re-prompts.
                0x03 => {
                    w(b"^C\n");
                    elen = 0;
                    echoed = 0;
                    if pending != HANDLE_NULL {
                        let _ = send_chunk(pending, &[], 0);
                        pending = HANDLE_NULL;
                    }
                }
                b'\n' | b'\r' => {
                    if pending != HANDLE_NULL {
                        // Reader waiting (line echoed live): deliver it now, chunked.
                        w(b"\n");
                        deliver[..elen].copy_from_slice(&edit[..elen]);
                        dvlen = elen;
                        dvoff = send_chunk(pending, &deliver[..dvlen], 0);
                        pending = HANDLE_NULL;
                    } else if dcount < DONE_CAP {
                        // Busy: queue un-echoed; echoed when a READ pops it.
                        let slot = (dhead + dcount) % DONE_CAP;
                        done[slot][..elen].copy_from_slice(&edit[..elen]);
                        dlen[slot] = elen;
                        dcount += 1;
                    } else {
                        w(b"[tty] !line dropped\n");
                    }
                    elen = 0;
                    echoed = 0;
                }
                c @ 0x20..=0x7E => {
                    if elen < MAX_LINE {
                        edit[elen] = c;
                        elen += 1;
                        if pending != HANDLE_NULL {
                            w(&[c]); // reader waiting: echo live, as before
                            echoed = elen;
                        }
                        // else: type-ahead — defer echo until the next READ
                    }
                }
                _ => {}
            },
            TAG_TTY_READ => {
                if dvoff < dvlen {
                    // Mid-delivery: stream the next chunk of the current line.
                    dvoff = send_chunk(reply, &deliver[..dvlen], dvoff);
                } else if dcount > 0 {
                    let slot = dhead;
                    dhead = (dhead + 1) % DONE_CAP;
                    dcount -= 1;
                    // Invariant 3: this line completed while the shell was busy and
                    // is un-echoed. The shell's prompt WRITE preceded this READ in
                    // FIFO order, so echoing now groups the line with its own prompt.
                    w(&done[slot][..dlen[slot]]);
                    w(b"\n");
                    deliver[..dlen[slot]].copy_from_slice(&done[slot][..dlen[slot]]);
                    dvlen = dlen[slot];
                    dvoff = send_chunk(reply, &deliver[..dvlen], 0);
                } else if pending != HANDLE_NULL {
                    // Only one shell expected; reply an empty line defensively.
                    let _ = send_chunk(reply, &[], 0);
                } else {
                    pending = reply; // stash until a line completes
                    // Flush any deferred type-ahead echo for the in-progress line.
                    // The prompt WRITE already printed (FIFO order), so the echo
                    // now appears after it, restoring invariant 1.
                    if echoed < elen {
                        w(&edit[echoed..elen]);
                        echoed = elen;
                    }
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
