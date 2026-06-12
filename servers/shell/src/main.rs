//! shell — the interactive command line (module 4). Each iteration: print the
//! `oxbow$ ` prompt, read a line from the tty, parse it, and run a builtin.
//! All output is routed back THROUGH the tty (TAG_TTY_WRITE) so the prompt,
//! the keystroke echo, and command output all serialize onto the one console
//! the tty owns. The shell needs no Console handle of its own (revoked in P5).
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_CONSOLE, BOOT_TTY, TAG_TTY_READ, TAG_TTY_WRITE};
use oxbow_rt as rt;

const PROMPT: &[u8] = b"oxbow$ ";

/// Write a byte string to the console via the tty. Chunks into <=63-byte,
/// NUL-terminated TAG_TTY_WRITE messages so payloads longer than one MsgBuf
/// (e.g. the help text) still go out whole.
fn tw(s: &[u8]) {
    let mut off = 0;
    while off < s.len() {
        let n = core::cmp::min(63, s.len() - off);
        let mut m = MsgBuf::new(TAG_TTY_WRITE);
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(s[off..].as_ptr(), dst, n);
            *dst.add(n) = 0;
        }
        m.data_len = ((n + 1 + 7) / 8) as u32;
        let _ = rt::sys_send(BOOT_TTY, &m);
        off += n;
    }
}

/// Read one line from the tty (blocks until Enter). Returns it in `buf`, length.
fn read_line(buf: &mut [u8; 64]) -> usize {
    let mut m = MsgBuf::new(TAG_TTY_READ);
    if rt::sys_call(BOOT_TTY, &mut m).is_err() {
        return 0;
    }
    let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
    let n = bytes.iter().position(|&b| b == 0).unwrap_or(0);
    buf[..n].copy_from_slice(&bytes[..n]);
    n
}

/// Split a line into (command, rest-of-line) at the first run of spaces.
/// `rest` keeps everything after the first space, leading spaces trimmed.
fn split_cmd(line: &[u8]) -> (&[u8], &[u8]) {
    let cmd_end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
    let (cmd, after) = line.split_at(cmd_end);
    // Trim leading spaces from the remainder.
    let mut i = 0;
    while i < after.len() && after[i] == b' ' {
        i += 1;
    }
    (cmd, &after[i..])
}

fn run(line: &[u8]) {
    let (cmd, rest) = split_cmd(line);
    match cmd {
        b"" => {}
        b"echo" => {
            tw(rest);
            tw(b"\n");
        }
        b"help" => {
            tw(b"oxbow shell builtins:\n");
            tw(b"  echo <text>   print text\n");
            tw(b"  help          this list\n");
        }
        _ => {
            tw(b"oxbow: ");
            tw(cmd);
            tw(b": command not found\n");
        }
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    tw(b"[sh] shell ready\n");
    // Negative self-check: the boot loader revoked our Console handle, so a
    // direct hardware write MUST fail. Prove it (reported through the tty).
    let probe = b"X";
    if rt::sys_console_write(BOOT_CONSOLE, probe.as_ptr(), 1).is_err() {
        tw(b"[sh] direct console write denied (revoked) ok\n");
    } else {
        tw(b"[sh] !! direct console write SUCCEEDED (revocation broken)\n");
    }
    let mut line = [0u8; 64];
    loop {
        tw(PROMPT);
        let n = read_line(&mut line);
        run(&line[..n]);
    }
}
