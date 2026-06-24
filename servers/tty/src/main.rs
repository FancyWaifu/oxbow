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
    Handle, MsgBuf, BOOT_CONSOLE, BOOT_TERM_CHAN, BOOT_TTY, HANDLE_NULL, TAG_TTY_CHAR,
    TAG_TTY_FLUSH, TAG_TTY_LINE, TAG_TTY_MODE, TAG_TTY_MUTE, TAG_TTY_READ, TAG_TTY_WRITE,
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
    // §53: mirror every byte of console output to the graphical terminal
    // (oxterm) over the terminal channel. Best-effort: if oxterm isn't there the
    // send just fails. oxterm drains continuously, so this does not block in
    // practice (the channel buffer dwarfs interactive output).
    let _ = rt::channel::send(BOOT_TERM_CHAN, s, &[]);
}

/// Reply one chunk of `line` starting at `off`: data[0] = chunk byte count,
/// data[1] = 1 if more chunks follow (else 0), payload at byte offset 16. The
/// shell loops READ, accumulating chunks until `more` is 0. Returns the new off.
/// Reply to a blocked reader with a zero-length control marker in data[1]:
/// 2 = EOF (Ctrl-D), 3 = interrupt (Ctrl-C). A normal line uses 0/1 (see send_chunk),
/// so readers distinguish "empty line" from "EOF"/"interrupted".
fn send_marker(reply: Handle, marker: u64) {
    let mut m = MsgBuf::new(TAG_TTY_LINE);
    m.data[0] = 0;
    m.data[1] = marker;
    m.data_len = 8;
    let _ = rt::sys_reply(reply, &m);
}

/// Reply with raw bytes (marker 5 in data[1]): the reader copies them verbatim with
/// no line/newline semantics. Used in raw mode for TUI keystroke delivery.
fn send_raw(reply: Handle, bytes: &[u8]) {
    let n = core::cmp::min(CHUNK, bytes.len());
    let mut m = MsgBuf::new(TAG_TTY_LINE);
    m.data[0] = n as u64;
    m.data[1] = 5;
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(16), n);
    }
    m.data_len = 8;
    let _ = rt::sys_reply(reply, &m);
}

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
    // Phase 7 raw mode (for TUI apps with ~ICANON, e.g. editors): keystrokes are
    // delivered to the reader byte-for-byte with no echo, editing, or signals. `raw`
    // is toggled by TAG_TTY_MODE; `rawbuf` holds bytes typed ahead of a READ.
    let mut raw = false;
    let mut rawbuf = [0u8; 256];
    let mut rawlen = 0usize;
    // §92 focus gating: while `muted`, drop kbd keystrokes (TAG_TTY_CHAR) so they go
    // only to the focused graphical window. The compositor toggles this on focus
    // changes (muted when a non-terminal window is focused). Shell READ/WRITE and
    // control messages are unaffected — only live keyboard input is suppressed.
    let mut muted = false;

    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_TTY, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match m.tag {
            // §92: toggle focus gating. data[0] != 0 -> mute (drop keystrokes).
            TAG_TTY_MUTE if m.data_len >= 1 => {
                muted = m.data[0] != 0;
            }
            // Focus is on a non-terminal window: swallow the keystroke (it reached the
            // focused app via wl_keyboard already). Drop silently — no echo, no buffer.
            TAG_TTY_CHAR if muted => {}
            // Phase 7: raw-mode keystroke — deliver byte-for-byte, no echo/editing/
            // signals. Ctrl-C/Ctrl-D are just bytes here (the app's ISIG is off).
            TAG_TTY_CHAR if raw && m.data_len >= 1 => {
                let b = m.data[0] as u8;
                if rawlen < rawbuf.len() {
                    rawbuf[rawlen] = b;
                    rawlen += 1;
                }
                if pending != HANDLE_NULL {
                    send_raw(pending, &rawbuf[..rawlen]);
                    pending = HANDLE_NULL;
                    rawlen = 0;
                }
            }
            // Switch line discipline (raw <-> cooked); reset both buffers.
            TAG_TTY_MODE if m.data_len >= 1 => {
                raw = m.data[0] != 0;
                elen = 0;
                echoed = 0;
                rawlen = 0;
            }
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
                        // Marker 3 = interrupt. A program reader turns this into a
                        // SIGINT; the shell treats it as a cancelled (empty) line and
                        // re-prompts. (Was an empty line — indistinguishable from a
                        // blank Enter, so a program couldn't see the ^C.)
                        send_marker(pending, 3);
                        pending = HANDLE_NULL;
                    } else {
                        // §Phase 9: no reader is blocked — a foreground program is
                        // RUNNING (not at a read boundary). Deliver async Ctrl-C: the
                        // kernel terminates the foreground process (default SIGINT).
                        rt::sys_tty_intr();
                    }
                }
                // Ctrl-D: EOF on an empty line; otherwise flush the partial line (no
                // Enter needed), like a Unix tty. Only meaningful with a waiting reader.
                0x04 => {
                    if pending != HANDLE_NULL {
                        if elen == 0 {
                            send_marker(pending, 2); // EOF
                        } else {
                            w(b"\n");
                            deliver[..elen].copy_from_slice(&edit[..elen]);
                            dvlen = elen;
                            dvoff = send_chunk(pending, &deliver[..dvlen], 0);
                        }
                        pending = HANDLE_NULL;
                        elen = 0;
                        echoed = 0;
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
            // Phase 7: raw read — return buffered keystrokes now, or stash the reply
            // and deliver the next keystroke the moment it arrives (VMIN=1 semantics).
            TAG_TTY_READ if raw => {
                if rawlen > 0 {
                    send_raw(reply, &rawbuf[..rawlen]);
                    rawlen = 0;
                } else {
                    pending = reply;
                }
            }
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
            // §92: drop all buffered input — the in-progress edit line, the queued
            // completed lines, and any mid-delivery. Used after a graphical login so
            // characters typed into the greeter (the kbd driver forwards keystrokes
            // here too) don't surface as phantom commands in the new session. A
            // pending reader is left stashed; only the buffered INPUT is discarded.
            TAG_TTY_FLUSH => {
                elen = 0;
                echoed = 0;
                dhead = 0;
                dcount = 0;
                dvlen = 0;
                dvoff = 0;
            }
            _ => {}
        }
    }
}
