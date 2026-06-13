//! shell — the interactive command line (module 4). Each iteration: print the
//! `oxbow$ ` prompt, read a line from the tty, parse it, and run a builtin.
//! All output is routed back THROUGH the tty (TAG_TTY_WRITE) so the prompt,
//! the keystroke echo, and command output all serialize onto the one console
//! the tty owns. The shell needs no Console handle of its own (revoked in P5).
#![no_std]
#![no_main]

use oxbow_abi::{
    Handle, MsgBuf, BOOT_CONSOLE, BOOT_IMG_BETA, BOOT_IMG_HELLO, BOOT_IMG_PONG, BOOT_MEM, BOOT_TICK,
    BOOT_TTY, HANDLE_NULL, R_GRANT, R_RECV, R_SEND, R_WAIT, TAG_TTY_READ, TAG_TTY_WRITE,
};
use oxbow_rt as rt;

const PROMPT: &[u8] = b"oxbow$ ";

/// Capabilities the shell mints once at startup to launch programs with.
struct Spawner {
    /// An attenuated tty send endpoint, handed to children as their stdout.
    stdout: Handle,
    /// A notification the kernel signals when a spawned child exits.
    exit: Handle,
    /// A spare endpoint used to wire up child↔child IPC (e.g. pong↔beta).
    ep: Handle,
}

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

/// Block until `n` spawned children have exited (the kernel signals `exit`,
/// a counting notification, once per death).
fn wait_exits(sp: &Spawner, n: u64) {
    let mut exited = 0u64;
    while exited < n {
        match rt::sys_notif_wait(sp.exit) {
            Ok(c) => exited += c,
            Err(_) => break,
        }
    }
}

/// Spawn a single self-contained program (just stdout), and wait for it to exit.
fn spawn_one(image: Handle, sp: &Spawner) {
    // Spawn MsgBuf (§13): data[0] = child budget (0 → default); the granted
    // handles land at the §13 slots — NULL skips slot 1, stdout lands at slot 2.
    let mut m = MsgBuf::new(0);
    m.data_len = 1;
    m.handle_count = 2;
    m.handles[0] = HANDLE_NULL; // slot 1: unused
    m.handles[1] = sp.stdout; // slot 2: SPAWN_STDOUT
    match rt::sys_spawn(image, BOOT_MEM, &m, sp.exit) {
        Ok(_) => wait_exits(sp, 1),
        Err(_) => tw(b"run: spawn failed\n"),
    }
}

/// `run pong`: launch the pong↔beta demo pair, wiring an endpoint between them
/// (beta gets the recv side, pong the send side) and delegating the tick to pong.
/// Proves multi-handle grant-at-spawn and child↔child IPC.
fn run_pong(sp: &Spawner) {
    let ep_recv = rt::sys_attenuate(sp.ep, R_RECV | R_GRANT);
    let ep_send = rt::sys_attenuate(sp.ep, R_SEND | R_GRANT);
    let tick_w = rt::sys_attenuate(BOOT_TICK, R_WAIT | R_GRANT);
    let (ep_recv, ep_send, tick_w) = match (ep_recv, ep_send, tick_w) {
        (Ok(r), Ok(s), Ok(t)) => (r, s, t),
        _ => {
            tw(b"run: could not set up pong channel\n");
            return;
        }
    };
    // beta (receiver) first, so it is ready to recv when pong sends.
    let mut mb = MsgBuf::new(0);
    mb.data_len = 1;
    mb.handle_count = 2;
    mb.handles[0] = ep_recv; // slot 1 = BOOT_EP (recv)
    mb.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    let beta_ok = rt::sys_spawn(BOOT_IMG_BETA, BOOT_MEM, &mb, sp.exit).is_ok();
    // pong (sender) gets the send endpoint, stdout, and the tick.
    let mut mp = MsgBuf::new(0);
    mp.data_len = 1;
    mp.handle_count = 3;
    mp.handles[0] = ep_send; // slot 1 = BOOT_EP (send)
    mp.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    mp.handles[2] = tick_w; // slot 4 = BOOT_TICK
    let pong_ok = rt::sys_spawn(BOOT_IMG_PONG, BOOT_MEM, &mp, sp.exit).is_ok();
    if !beta_ok || !pong_ok {
        tw(b"run: pong spawn failed\n");
    }
    wait_exits(sp, beta_ok as u64 + pong_ok as u64);
    // Release the per-run attenuated handles (the children hold their own copies).
    let _ = rt::sys_close(ep_recv);
    let _ = rt::sys_close(ep_send);
    let _ = rt::sys_close(tick_w);
}

fn run(line: &[u8], sp: &Spawner) {
    let (cmd, rest) = split_cmd(line);
    match cmd {
        b"" => {}
        b"echo" => {
            tw(rest);
            tw(b"\n");
        }
        b"run" => {
            let (prog, _) = split_cmd(rest);
            match prog {
                b"hello" => spawn_one(BOOT_IMG_HELLO, sp),
                b"pong" => run_pong(sp),
                b"" => tw(b"run: usage: run <program>\n"),
                _ => tw(b"run: no such program\n"),
            }
        }
        b"help" => {
            tw(b"oxbow shell builtins:\n");
            tw(b"  echo <text>     print text\n");
            tw(b"  run hello       spawn the hello program\n");
            tw(b"  run pong        spawn the pong<->beta IPC demo\n");
            tw(b"  help            this list\n");
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
    // Mint the spawn capabilities once: an attenuated send-only "stdout" endpoint
    // to hand children (BOOT_TTY keeps R_GRANT so we can pass it on), and one exit
    // notification reused for every spawn.
    let sp = Spawner {
        stdout: rt::sys_attenuate(BOOT_TTY, R_SEND | R_GRANT).unwrap_or(HANDLE_NULL),
        exit: rt::sys_notif_create().unwrap_or(HANDLE_NULL),
        ep: rt::sys_ep_create().unwrap_or(HANDLE_NULL),
    };
    let mut line = [0u8; 64];
    loop {
        tw(PROMPT);
        let n = read_line(&mut line);
        run(&line[..n], &sp);
    }
}
