//! shell — the interactive command line (module 4). Each iteration: print the
//! `oxbow$ ` prompt, read a line from the tty, parse it, and run a builtin.
//! All output is routed back THROUGH the tty (TAG_TTY_WRITE) so the prompt,
//! the keystroke echo, and command output all serialize onto the one console
//! the tty owns. The shell needs no Console handle of its own (revoked in P5).
#![no_std]
#![no_main]

use oxbow_abi::{
    Handle, MsgBuf, SysError, BOOT_CONSOLE, BOOT_FS_ROOT, BOOT_IMG_BADGE, BOOT_IMG_BETA, BOOT_NET_EP,
    BOOT_IMG_CAT, BOOT_IMG_CP, BOOT_IMG_HELLO, BOOT_IMG_LS, BOOT_IMG_MKDIR, BOOT_IMG_MV, BOOT_IMG_PONG,
    BOOT_IMG_RM, BOOT_IMG_TOUCH, BOOT_MEM, BOOT_TICK, BOOT_TTY, HANDLE_NULL, R_GRANT, R_RECV,
    R_SEND, R_WAIT, R_WRITE, TAG_FS_CREATE, TAG_FS_OPEN, TAG_FS_WRITE, TAG_TTY_READ, TAG_TTY_WRITE,
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

/// Spawn a program, granting it `cap0` at slot 1 (BOOT_EP) and stdout at slot 2,
/// then wait for it to exit. `cap0 = HANDLE_NULL` for a program that needs no
/// input capability (e.g. hello). For ls/cat, `cap0` is the dir/file capability
/// the shell hands over — the spawned coreutil never sees a name, just the cap.
fn spawn_with(image: Handle, cap0: Handle, arg: &[u8], sp: &Spawner) {
    let mut m = MsgBuf::new(0);
    // data[0] = budget (0 = default); the argument string rides in data[1..]
    // (byte offset 8), NUL-terminated — the kernel maps it at SPAWN_ARGV (§13).
    let n = core::cmp::min(arg.len(), 55);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(arg.as_ptr(), dst.add(8), n);
        *dst.add(8 + n) = 0;
    }
    m.data_len = 8;
    m.handle_count = 2;
    m.handles[0] = cap0; // slot 1 = BOOT_EP (a file/dir cap, or NULL)
    m.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    match rt::sys_spawn(image, BOOT_MEM, &m, sp.exit) {
        Ok(_) => wait_exits(sp, 1),
        Err(_) => tw(b"run: spawn failed\n"),
    }
}

/// `ls [path]`: with no arg, list `dir`; with a path, OPEN it (must be a
/// directory) and hand that capability to a spawned `ls`.
fn ls_cmd(dir: Handle, path: &[u8], sp: &Spawner) {
    if path.is_empty() {
        spawn_with(BOOT_IMG_LS, dir, b"", sp);
        return;
    }
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, path);
    if rt::sys_call(dir, &mut m).is_err() || m.data[0] != 0 {
        tw(b"ls: ");
        tw(path);
        tw(b": no such directory\n");
        return;
    }
    let cap = m.handles[0];
    if m.data[1] != oxbow_abi::FS_DIR {
        tw(b"ls: ");
        tw(path);
        tw(b": not a directory\n");
    } else {
        spawn_with(BOOT_IMG_LS, cap, b"", sp);
    }
    let _ = rt::sys_close(cap);
}

/// `cat <name>`: resolve the name relative to `dir` (the shell holds the dir cap),
/// then hand the resulting FILE capability to a freshly-spawned `cat` program —
/// which reads exactly that one file and nothing else.
fn cat_cmd(dir: Handle, name: &[u8], sp: &Spawner) {
    if name.is_empty() {
        tw(b"cat: usage: cat <file>\n");
        return;
    }
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, name);
    if rt::sys_call(dir, &mut m).is_err() || m.data[0] != 0 {
        tw(b"cat: ");
        tw(name);
        tw(b": not found\n");
        return;
    }
    let file_cap = m.handles[0];
    spawn_with(BOOT_IMG_CAT, file_cap, b"", sp); // cat reads the file cap, prints, exits
    let _ = rt::sys_close(file_cap); // cat holds its own copy
}

/// Write a byte as decimal ASCII to the tty (for printing IP octets).
fn tw_dec(n: u8) {
    let mut b = [0u8; 3];
    let mut i = 3;
    let mut v = n;
    loop {
        i -= 1;
        b[i] = b'0' + v % 10;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    tw(&b[i..]);
}

/// `dns <hostname>`: resolve a name over the net server's UDP socket capability
/// API. We hold only BOOT_NET_EP (the NET_CTL control cap); `udp::bind` mints us
/// a socket cap, and the whole DNS exchange rides it — net never sees a name,
/// just a UDP datagram on a bound port. This crosses the shell↔net process
/// boundary entirely through capabilities.
fn dns_cmd(name: &[u8]) {
    if name.is_empty() {
        tw(b"dns: usage: dns <hostname>\n");
        return;
    }
    let Ok(name_str) = core::str::from_utf8(name) else {
        tw(b"dns: bad name\n");
        return;
    };
    let Some((sock, _port)) = rt::udp::bind(BOOT_NET_EP, 0) else {
        tw(b"dns: bind failed (no net server?)\n");
        return;
    };
    let q = rt::dns::query(0x1234, name_str);
    if !rt::udp::sendto(sock, [10, 0, 2, 3], 53, &q) {
        tw(b"dns: send failed\n");
        let _ = rt::sys_close(sock);
        return;
    }
    let mut buf = [0u8; 64];
    let n = rt::udp::recvfrom(sock, &mut buf);
    let _ = rt::sys_close(sock);
    match rt::dns::first_a(&buf[..n]) {
        Some(ip) => {
            tw(name);
            tw(b" -> ");
            tw_dec(ip[0]);
            tw(b".");
            tw_dec(ip[1]);
            tw(b".");
            tw_dec(ip[2]);
            tw(b".");
            tw_dec(ip[3]);
            tw(b"\n");
        }
        None => {
            tw(name);
            tw(b": no A record\n");
        }
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

/// `badgetest`: exercise the §14 badged-endpoint mint rules from the shell.
/// Phase 2 = the negative paths (the end-to-end delivery demo is added next).
fn badgetest(sp: &Spawner) {
    // Two distinct badges minted off our (unbadged, R_ATTENUATE-bearing) ep.
    let b7 = rt::sys_mint(sp.ep, 7, R_SEND);
    let b42 = rt::sys_mint(sp.ep, 42, R_SEND);
    match (b7, b42) {
        (Ok(_), Ok(_)) => tw(b"[sh] mint 7+42 ok\n"),
        _ => tw(b"[sh] !! mint failed\n"),
    }
    // Re-badging an already-badged cap is forbidden (unforgeability).
    if let Ok(b) = b7 {
        match rt::sys_mint(b, 99, R_SEND) {
            Err(SysError::Rights) => tw(b"[sh] re-badge denied ok\n"),
            _ => tw(b"[sh] !! re-badge NOT denied\n"),
        }
    }
    // Badge 0 is reserved for "unbadged".
    match rt::sys_mint(sp.ep, 0, R_SEND) {
        Err(SysError::Msg) => tw(b"[sh] badge 0 denied ok\n"),
        _ => tw(b"[sh] !! badge 0 NOT denied\n"),
    }
    // Amplification (a right the source lacks) is refused (law L5).
    match rt::sys_mint(sp.ep, 5, R_SEND | R_WRITE) {
        Err(SysError::Rights) => tw(b"[sh] amplify denied ok\n"),
        _ => tw(b"[sh] !! amplify NOT denied\n"),
    }
    // Minting a non-endpoint is a type error.
    match rt::sys_mint(BOOT_MEM, 7, 0) {
        Err(SysError::BadType) => tw(b"[sh] non-ep denied ok\n"),
        _ => tw(b"[sh] !! non-ep NOT denied\n"),
    }

    // --- end-to-end: spawn the badge server and prove delivery + unforgeability.
    let (b7, b42) = match (b7, b42) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return,
    };
    // The server receives on our endpoint; grant it the recv side at slot 1.
    let recv_cap = match rt::sys_attenuate(sp.ep, R_RECV | R_GRANT) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut m = MsgBuf::new(0);
    m.handle_count = 2;
    m.handles[0] = recv_cap; // slot 1 = BOOT_EP (recv)
    m.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    if rt::sys_spawn(BOOT_IMG_BADGE, BOOT_MEM, &m, sp.exit).is_ok() {
        // Send through each badged cap → server should report 7 then 42.
        let p = MsgBuf::new(0);
        let _ = rt::sys_send(b7, &p);
        let _ = rt::sys_send(b42, &p);
        // Forgery attempt: write a badge into the message and send via the
        // UNBADGED ep — the kernel overwrites it, so the server must report 0.
        let mut forged = MsgBuf::new(0);
        forged.badge = 1234;
        let _ = rt::sys_send(sp.ep, &forged);
        wait_exits(sp, 1);
    }
    let _ = rt::sys_close(recv_cap);
    let _ = rt::sys_close(b7);
    let _ = rt::sys_close(b42);
}

/// Strip leading and trailing spaces.
fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && s[a] == b' ' {
        a += 1;
    }
    while b > a && s[b - 1] == b' ' {
        b -= 1;
    }
    &s[a..b]
}

/// Pack a NUL-terminated name into a request MsgBuf's data.
fn pack_name(m: &mut MsgBuf, name: &[u8]) {
    let n = core::cmp::min(name.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
}

/// WRITE `bytes` to the file `cap` starting at `start`, looping in <=48-byte
/// chunks. Returns the next write offset.
fn write_chunks(cap: Handle, bytes: &[u8], start: u64) -> u64 {
    let mut off = start;
    let mut i = 0;
    while i < bytes.len() {
        let n = core::cmp::min(48, bytes.len() - i);
        let mut wm = MsgBuf::new(TAG_FS_WRITE);
        wm.data[0] = off;
        wm.data[1] = n as u64;
        let dst = wm.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(bytes[i..].as_ptr(), dst.add(16), n) };
        wm.data_len = 8;
        if rt::sys_call(cap, &mut wm).is_err() {
            break;
        }
        let wrote = wm.data[0] as usize;
        if wrote == 0 {
            break; // out of space
        }
        off += wrote as u64;
        i += wrote;
    }
    off
}

/// `echo TEXT > FILE`: CREATE-or-truncate the file (relative to `dir`), write
/// TEXT + newline.
fn write_file(dir: Handle, name: &[u8], text: &[u8], append: bool) {
    if name.is_empty() {
        tw(b"sh: redirect needs a file name\n");
        return;
    }
    // Append mode: OPEN the file and write at its current end. If it doesn't
    // exist (or for '>'), CREATE-or-truncate and write at 0.
    let (cap, start) = if append {
        let mut o = MsgBuf::new(TAG_FS_OPEN);
        pack_name(&mut o, name);
        if rt::sys_call(dir, &mut o).is_ok()
            && o.data[0] == 0
            && o.data[1] == oxbow_abi::FS_FILE
        {
            (o.handles[0], o.data[2]) // existing file: append at its size
        } else {
            let mut c = MsgBuf::new(TAG_FS_CREATE);
            pack_name(&mut c, name);
            if rt::sys_call(dir, &mut c).is_err() || c.data[0] != 0 {
                tw(b"sh: cannot create ");
                tw(name);
                tw(b"\n");
                return;
            }
            (c.handles[0], 0)
        }
    } else {
        let mut c = MsgBuf::new(TAG_FS_CREATE);
        pack_name(&mut c, name);
        if rt::sys_call(dir, &mut c).is_err() || c.data[0] != 0 {
            tw(b"sh: cannot create ");
            tw(name);
            tw(b"\n");
            return;
        }
        (c.handles[0], 0)
    };
    let off = write_chunks(cap, text, start);
    let _ = write_chunks(cap, b"\n", off);
    let _ = rt::sys_close(cap);
}

/// `cd <name>` / `cd /`: change the current-directory capability. `cd` with no
/// arg (or `/`) returns to the root; `cd <name>` opens a subdir relative to the
/// current one. Confinement: there is no `cd ..` — you can't walk above a dir cap
/// you hold; `cd /` works only because the shell still holds the root cap.
fn cd(name: &[u8], cwd: &mut Handle) {
    if name.is_empty() || name == b"/" {
        if *cwd != BOOT_FS_ROOT {
            let _ = rt::sys_close(*cwd);
        }
        *cwd = BOOT_FS_ROOT;
        return;
    }
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, name);
    if rt::sys_call(*cwd, &mut m).is_err() || m.data[0] != 0 {
        tw(b"cd: ");
        tw(name);
        tw(b": no such directory\n");
        return;
    }
    let cap = m.handles[0];
    if m.data[1] != oxbow_abi::FS_DIR {
        tw(b"cd: ");
        tw(name);
        tw(b": not a directory\n");
        let _ = rt::sys_close(cap);
        return;
    }
    if *cwd != BOOT_FS_ROOT {
        let _ = rt::sys_close(*cwd);
    }
    *cwd = cap;
}

fn run(line: &[u8], sp: &Spawner, cwd: &mut Handle) {
    // Output redirect: `echo TEXT > FILE` (truncate) or `>> FILE` (append).
    if let Some(gt) = line.iter().position(|&b| b == b'>') {
        let append = gt + 1 < line.len() && line[gt + 1] == b'>';
        let file_start = if append { gt + 2 } else { gt + 1 };
        let (cmd, text) = split_cmd(trim(&line[..gt]));
        let (file, _) = split_cmd(trim(&line[file_start..]));
        if cmd == b"echo" {
            write_file(*cwd, file, text, append);
        } else {
            tw(b"sh: only 'echo ... > file' redirect is supported\n");
        }
        return;
    }
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
                b"hello" => spawn_with(BOOT_IMG_HELLO, HANDLE_NULL, b"", sp),
                b"pong" => run_pong(sp),
                b"" => tw(b"run: usage: run <program>\n"),
                _ => tw(b"run: no such program\n"),
            }
        }
        b"ls" => ls_cmd(*cwd, rest, sp),
        b"cat" => cat_cmd(*cwd, rest, sp),
        b"mkdir" => spawn_with(BOOT_IMG_MKDIR, *cwd, rest, sp),
        b"touch" => spawn_with(BOOT_IMG_TOUCH, *cwd, rest, sp),
        b"rm" => spawn_with(BOOT_IMG_RM, *cwd, rest, sp),
        b"mv" => spawn_with(BOOT_IMG_MV, *cwd, rest, sp),
        b"cp" => spawn_with(BOOT_IMG_CP, *cwd, rest, sp),
        b"cd" => cd(rest, cwd),
        b"dns" => dns_cmd(rest),
        b"badgetest" => badgetest(sp),
        b"help" => {
            tw(b"oxbow shell:  (ls cat mkdir touch are spawned programs)\n");
            tw(b"  echo <text>     print text (echo .. > f redirects to a file)\n");
            tw(b"  ls              list the current directory\n");
            tw(b"  cat <file>      print a file\n");
            tw(b"  mkdir <name>    make a directory\n");
            tw(b"  touch <name>    make an empty file\n");
            tw(b"  rm <name>       remove a file or empty dir\n");
            tw(b"  mv <old> <new>  rename within the directory\n");
            tw(b"  cp <src> <dst>  copy a file\n");
            tw(b"  cd <dir> | /    change directory (builtin)\n");
            tw(b"  dns <host>      resolve a hostname via the net UDP socket API\n");
            tw(b"  run hello/pong  spawn a demo program\n");
            tw(b"  badgetest       exercise badged-endpoint mint rules\n");
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
    // The current-directory capability (starts at the filesystem root).
    let mut cwd: Handle = BOOT_FS_ROOT;
    let mut line = [0u8; 64];
    loop {
        tw(PROMPT);
        let n = read_line(&mut line);
        run(&line[..n], &sp, &mut cwd);
    }
}
