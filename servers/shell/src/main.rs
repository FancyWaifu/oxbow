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
    BOOT_IMG_CCHELLO, BOOT_IMG_DRIFT, BOOT_IMG_TCC, BOOT_IMG_LUA, BOOT_IMG_UPY, BOOT_IMG_QJS, BOOT_IMG_RM, BOOT_IMG_TOUCH, BOOT_MEM, BOOT_TICK, BOOT_TTY,
    HANDLE_NULL, R_GRANT, R_RECV,
    R_SEND, R_WAIT, R_WRITE, TAG_FS_CREATE, TAG_FS_OPEN, TAG_FS_WRITE, TAG_TTY_READ, TAG_TTY_WRITE,
};
use oxbow_rt as rt;

/// The current-directory path string, tracked alongside the cwd capability so
/// the prompt can show it (a Unix shell shows where you are).
#[derive(Clone, Copy)]
struct Path {
    buf: [u8; 128],
    len: usize,
}
impl Path {
    fn root() -> Self {
        let mut buf = [0u8; 128];
        buf[0] = b'/';
        Path { buf, len: 1 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    fn push(&mut self, comp: &[u8]) {
        if self.buf[self.len - 1] != b'/' && self.len < self.buf.len() {
            self.buf[self.len] = b'/';
            self.len += 1;
        }
        for &c in comp {
            if self.len < self.buf.len() {
                self.buf[self.len] = c;
                self.len += 1;
            }
        }
    }
    fn pop(&mut self) {
        if self.len <= 1 {
            self.len = 1;
            return;
        }
        let mut i = self.len;
        while i > 1 && self.buf[i - 1] != b'/' {
            i -= 1;
        }
        self.len = if i > 1 { i - 1 } else { 1 };
    }
    /// Update the path for a `cd` target (handles `/`, `..`, `.`, multi-component).
    fn apply(&mut self, name: &[u8]) {
        if name.is_empty() || name == b"/" {
            self.len = 1;
            self.buf[0] = b'/';
            return;
        }
        if name[0] == b'/' {
            self.len = 1;
            self.buf[0] = b'/';
        }
        for comp in name.split(|&b| b == b'/') {
            match comp {
                b"" | b"." => {}
                b".." => self.pop(),
                _ => self.push(comp),
            }
        }
    }
}

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
    spawn_with_budget(image, cap0, arg, 0, sp);
}

/// Like `spawn_with`, but requests a specific child Memory budget (0 = default).
/// tcc needs a large working set to compile, so it asks for a big budget.
fn spawn_with_budget(image: Handle, cap0: Handle, arg: &[u8], budget: u64, sp: &Spawner) {
    let mut m = MsgBuf::new(0);
    // data[0] = budget (0 = default). Real argv (§13): data[1] = pointer to the
    // argument string, data[2] = its length — the kernel copies it into the
    // child's argv page (up to a full page, lifting the old 55-byte limit). `arg`
    // stays valid for this synchronous spawn call.
    m.data[0] = budget;
    m.data[1] = arg.as_ptr() as u64;
    m.data[2] = arg.len() as u64;
    m.data_len = 3;
    m.handle_count = 4;
    m.handles[0] = cap0; // slot 1 = BOOT_EP (a file/dir cap, or NULL)
    m.handles[1] = sp.stdout; // slot 2 = SPAWN_STDOUT
    m.handles[2] = HANDLE_NULL; // slot 4 = BOOT_TICK (unused here)
    m.handles[3] = BOOT_NET_EP; // slot 20 = BOOT_NET_EP (network access)
    match rt::sys_spawn(image, BOOT_MEM, &m, sp.exit) {
        Ok(_) => wait_exits(sp, 1),
        Err(_) => tw(b"run: spawn failed\n"),
    }
}

/// `cc <src> -o <out>`: compile + statically link a C program to a STANDALONE
/// oxbow binary via tcc. Expands to `tcc -static <args> /lib/c.a` — `/lib/c.a` is
/// liboxbow_libc.a, the C library archive that makes the output self-contained
/// (no dynamic linker; tcc fills the GOT at link time). `-static` is essential:
/// tcc defaults to a dynamic executable whose GOT a runtime ld.so would fill, but
/// oxbow has none. Run the result with `exec <out>`. The whole toolchain runs on
/// oxbow — the self-hosting milestone (ABI §35).
fn cc_cmd(cwd: Handle, rest: &[u8], sp: &Spawner) {
    if rest.is_empty() {
        tw(b"cc: usage: cc <src.c> -o <out>   (then: exec <out>)\n");
        return;
    }
    let prefix: &[u8] = b"-static ";
    let suffix: &[u8] = b" /lib/c.a";
    let mut arg = [0u8; 1024];
    if prefix.len() + rest.len() + suffix.len() > arg.len() {
        tw(b"cc: command too long\n");
        return;
    }
    let mut p = 0;
    for src in [prefix, rest, suffix] {
        arg[p..p + src.len()].copy_from_slice(src);
        p += src.len();
    }
    spawn_with_budget(BOOT_IMG_TCC, cwd, &arg[..p], 48 * 1024 * 1024, sp);
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

/// Scratch buffer holding an ELF read off the filesystem for `exec` (§33). Sized
/// for a stripped no_std binary with headroom; a larger image truncates safely
/// (read_all stops at the buffer end and try_validate then rejects it).
const ELF_BUF_CAP: usize = 2 * 1024 * 1024;
static mut ELF_BUF: [u8; ELF_BUF_CAP] = [0; ELF_BUF_CAP];

/// Slurp an entire file capability into `buf` via the 56-byte FS_READ protocol,
/// looping on the read offset until EOF. Returns the byte count read.
unsafe fn read_all(cap: Handle, buf: &mut [u8]) -> usize {
    let mut off = 0usize;
    loop {
        let mut m = MsgBuf::new(oxbow_abi::TAG_FS_READ);
        m.data[0] = off as u64;
        m.data_len = 1;
        if rt::sys_call(cap, &mut m).is_err() {
            break;
        }
        let count = core::cmp::min(m.data[0] as usize, 56);
        if count == 0 || off + count > buf.len() {
            break;
        }
        core::ptr::copy_nonoverlapping(
            (m.data.as_ptr() as *const u8).add(8),
            buf.as_mut_ptr().add(off),
            count,
        );
        off += count;
    }
    off
}

/// `exec <path> [args]`: read the ELF at `path` from the filesystem into a
/// buffer and launch it as a fresh process via exec-from-fs (ABI §33). Unlike
/// `ls`/`cat`/`tcc` — which spawn fixed boot-granted images — this runs an
/// ARBITRARY program loaded from disk: the foundation for running compiled
/// binaries. The program is granted the cwd dir cap (slot 1) and stdout
/// (slot 2), with the remaining tokens passed as its argv.
fn exec_cmd(cwd: Handle, path: &Path, arg_line: &[u8], sp: &Spawner) {
    let (pathname, rest) = split_cmd(arg_line);
    if pathname.is_empty() {
        tw(b"exec: usage: exec <path> [args]\n");
        return;
    }
    // Resolve to an absolute path and OPEN it from the root cap (the shell holds
    // root, so any absolute path resolves; the fs walks multi-component paths).
    let mut target = *path;
    target.apply(pathname);
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, target.as_bytes());
    if rt::sys_call(BOOT_FS_ROOT, &mut m).is_err() || m.data[0] != 0 {
        tw(b"exec: ");
        tw(pathname);
        tw(b": not found\n");
        return;
    }
    let file_cap = m.handles[0];
    if m.data[1] != oxbow_abi::FS_FILE {
        tw(b"exec: ");
        tw(pathname);
        tw(b": not a file\n");
        let _ = rt::sys_close(file_cap);
        return;
    }
    let len = unsafe {
        let buf = core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(ELF_BUF) as *mut u8, ELF_BUF_CAP);
        read_all(file_cap, buf)
    };
    let _ = rt::sys_close(file_cap);
    if len == 0 {
        tw(b"exec: empty or unreadable file\n");
        return;
    }
    // Build the spawn MsgBuf: budget default, argv from `rest` by (ptr, len),
    // cwd at slot 1, stdout at slot 2 — then hand the ELF bytes to the kernel.
    let mut sm = MsgBuf::new(0);
    sm.data[0] = 0;
    sm.data[1] = rest.as_ptr() as u64;
    sm.data[2] = rest.len() as u64;
    sm.data_len = 3;
    sm.handle_count = 4;
    sm.handles[0] = cwd;
    sm.handles[1] = sp.stdout;
    sm.handles[2] = HANDLE_NULL; // slot 4 (unused)
    sm.handles[3] = BOOT_NET_EP; // slot 20 = BOOT_NET_EP (network access)
    let elf = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(ELF_BUF) as *const u8, len) };
    match rt::sys_spawn_bytes(elf, BOOT_MEM, &sm, sp.exit) {
        Ok(_) => wait_exits(sp, 1),
        Err(_) => {
            tw(b"exec: not a valid program (spawn rejected, ");
            tw_dec_u32(len as u32);
            tw(b" bytes read)\n");
        }
    }
}

/// Write a u32 as decimal ASCII to the tty.
fn tw_dec_u32(mut n: u32) {
    let mut b = [0u8; 10];
    let mut i = 10;
    loop {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    tw(&b[i..]);
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

/// Parse a dotted-quad IPv4 address.
fn parse_ip(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    let mut val: u32 = 0;
    let mut have = false;
    for &c in s {
        if c == b'.' {
            if !have || idx >= 3 {
                return None;
            }
            octets[idx] = val as u8;
            idx += 1;
            val = 0;
            have = false;
        } else if c.is_ascii_digit() {
            val = val * 10 + (c - b'0') as u32;
            if val > 255 {
                return None;
            }
            have = true;
        } else {
            return None;
        }
    }
    if !have || idx != 3 {
        return None;
    }
    octets[3] = val as u8;
    Some(octets)
}

/// `http <ip>`: open a TCP connection to <ip>:80 over the net server's socket
/// capability API (smoltcp does the TCP), send a minimal HTTP/1.0 GET, and print
/// the response. We hold only BOOT_NET_EP; `tcp::connect` mints us a socket cap.
fn http_cmd(args: &[u8]) {
    let (host, rest) = split_cmd(args);
    let Some(ip) = parse_ip(host) else {
        tw(b"http: usage: http <a.b.c.d> [port]\n");
        return;
    };
    let (port_tok, _) = split_cmd(rest);
    let mut port: u16 = 80;
    if !port_tok.is_empty() {
        let mut v: u32 = 0;
        for &c in port_tok {
            if c.is_ascii_digit() {
                v = v * 10 + (c - b'0') as u32;
            }
        }
        if v > 0 && v <= 65535 {
            port = v as u16;
        }
    }
    let Some(sock) = rt::tcp::connect(BOOT_NET_EP, ip, port) else {
        tw(b"http: connect failed (refused/timeout)\n");
        return;
    };
    tw(b"http: connected, GET /\n");
    if !rt::tcp::send(sock, b"GET / HTTP/1.0\r\n\r\n") {
        tw(b"http: send failed\n");
        rt::tcp::close(sock);
        return;
    }
    let mut buf = [0u8; 64];
    let mut total = 0usize;
    for _ in 0..8 {
        let n = rt::tcp::recv(sock, &mut buf);
        if n == 0 {
            break;
        }
        tw(&buf[..n]);
        total += n;
    }
    if total == 0 {
        tw(b"http: no response\n");
    } else {
        tw(b"\n");
    }
    rt::tcp::close(sock);
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
fn cd(name: &[u8], cwd: &mut Handle, path: &mut Path) {
    // Normalize to an absolute target (handles `..`, `.`, absolute + relative),
    // then re-resolve it FROM ROOT. The fs forbids `..` within a directory cap
    // (you can't escape a capability), but the shell holds the root cap, so it
    // can always resolve any absolute path — that's what makes `cd ..` work.
    let mut target = *path;
    target.apply(name);
    let commit = |cwd: &mut Handle, cap: Handle| {
        if *cwd != BOOT_FS_ROOT {
            let _ = rt::sys_close(*cwd);
        }
        *cwd = cap;
    };
    if target.len == 1 {
        commit(cwd, BOOT_FS_ROOT);
        *path = target;
        return;
    }
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, target.as_bytes());
    if rt::sys_call(BOOT_FS_ROOT, &mut m).is_err() || m.data[0] != 0 {
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
    commit(cwd, cap);
    *path = target;
}

fn run(line: &[u8], sp: &Spawner, cwd: &mut Handle, path: &mut Path) {
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
        b"cd" => cd(rest, cwd, path),
        b"dns" => dns_cmd(rest),
        b"http" => http_cmd(rest),
        b"drift" => spawn_with(BOOT_IMG_DRIFT, HANDLE_NULL, rest, sp),
        b"cc-hello" => spawn_with(BOOT_IMG_CCHELLO, *cwd, rest, sp),
        b"tcc" => spawn_with_budget(BOOT_IMG_TCC, *cwd, rest, 48 * 1024 * 1024, sp),
        b"cc" => cc_cmd(*cwd, rest, sp),
        b"lua" => spawn_with_budget(BOOT_IMG_LUA, *cwd, rest, 32 * 1024 * 1024, sp),
        b"py" | b"micropython" => spawn_with_budget(BOOT_IMG_UPY, *cwd, rest, 32 * 1024 * 1024, sp),
        b"js" | b"qjs" => spawn_with_budget(BOOT_IMG_QJS, *cwd, rest, 48 * 1024 * 1024, sp),
        b"exec" => exec_cmd(*cwd, path, rest, sp),
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
            tw(b"  http <ip>       TCP GET / from <ip>:80 via the net socket API\n");
            tw(b"  drift           DRIFT crypto self-test (X25519/ChaCha20, needs SSE)\n");
            tw(b"  run hello/pong  spawn a demo program\n");
            tw(b"  cc <src> -o <o> compile+link a C file to a standalone binary (tcc -static)\n");
            tw(b"  lua [file.lua]  run the Lua 5.4 interpreter (built-in test, or a file)\n");
            tw(b"  py [file.py]    run MicroPython (built-in test, or a .py file)\n");
            tw(b"  js [file.js]    run QuickJS JavaScript (built-in test, or a .js file)\n");
            tw(b"  exec <path>     load + run an ELF from the filesystem (exec-from-fs)\n");
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
    // The current-directory capability + its path string (starts at the root).
    let mut cwd: Handle = BOOT_FS_ROOT;
    let mut path = Path::root();
    let mut line = [0u8; 64];
    loop {
        // Path-aware prompt, e.g. `oxbow:/usr/src$ `.
        tw(b"oxbow:");
        tw(path.as_bytes());
        tw(b"$ ");
        let n = read_line(&mut line);
        run(&line[..n], &sp, &mut cwd, &mut path);
    }
}
