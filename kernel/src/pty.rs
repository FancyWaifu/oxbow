//! pty — kernel pseudo-terminals (§102): the bidirectional, line-disciplined pipe a
//! terminal emulator (master) and a shell (slave) share. Unlike a plain `pipe`, a pty
//! owns the termios + the line discipline, so the SLAVE side is a real tty: cooked-mode
//! echo + canonical line editing, raw-mode passthrough for TUIs, OPOST output mapping,
//! winsize. This is what makes `isatty()` true and an interactive shell work.
//!
//! Two ring buffers per pty:
//!   to_master  — shell OUTPUT (slave write → master read), OPOST-processed.
//!   to_slave   — terminal INPUT (master write → slave read), but only COOKED data:
//!                in canonical mode a typed line buffers in `canon` and is released
//!                here on Enter; in raw mode bytes pass straight through.
//! Echo (typed chars rendered back in the terminal) is pushed to `to_master` by the
//! line discipline as input arrives.
//!
//! Like `pipe`, this module owns only the buffers + queues under one lock (never held
//! across a block); the syscall layer drives the block/retry + wake loop.
use crate::sync::DiagMutex;

const NPTYS: usize = 4;
const RBUF: usize = 4096; // per-direction ring
const LINE: usize = 256; // canonical line buffer
const QCAP: usize = 8;

// ---- termios bits we honor (Linux values) ----
const ICRNL: u32 = 0o0000400; // map CR→NL on input
const INLCR: u32 = 0o0000100; // map NL→CR on input
const OPOST: u32 = 0o0000001; // enable output processing
const ONLCR: u32 = 0o0000004; // map NL→CRNL on output
const ISIG: u32 = 0o0000001; // generate signals (INTR/QUIT)
const ICANON: u32 = 0o0000002; // canonical (line) mode
const ECHO: u32 = 0o0000010; // echo input
const ECHOE: u32 = 0o0000020; // echo erase as BS-SP-BS
const ECHOK: u32 = 0o0000040; // echo KILL by erasing the line
const ECHONL: u32 = 0o0000100; // echo NL even without ECHO
// c_cc indices
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const NCCS: usize = 19;

#[derive(Clone, Copy)]
struct WaitQ {
    q: [usize; QCAP],
    n: usize,
}
impl WaitQ {
    const fn new() -> Self {
        WaitQ { q: [0; QCAP], n: 0 }
    }
    fn push(&mut self, tid: usize) {
        if self.n < QCAP && !self.q[..self.n].contains(&tid) {
            self.q[self.n] = tid;
            self.n += 1;
        }
    }
    fn drain(&mut self, out: &mut [usize; QCAP]) -> usize {
        let n = self.n;
        out[..n].copy_from_slice(&self.q[..n]);
        self.n = 0;
        n
    }
    fn remove(&mut self, tid: usize) {
        if let Some(pos) = self.q[..self.n].iter().position(|&t| t == tid) {
            self.n -= 1;
            self.q[pos] = self.q[self.n];
        }
    }
}

#[derive(Clone, Copy)]
struct Ring {
    buf: [u8; RBUF],
    head: usize,
    len: usize,
}
impl Ring {
    const fn new() -> Self {
        Ring { buf: [0; RBUF], head: 0, len: 0 }
    }
    /// Push as many of `src` as fit (a tty DROPS on overflow rather than blocking).
    fn push(&mut self, src: &[u8]) {
        for &b in src {
            if self.len >= RBUF {
                break;
            }
            let pos = (self.head + self.len) % RBUF;
            self.buf[pos] = b;
            self.len += 1;
        }
    }
    fn pop(&mut self, out: &mut [u8]) -> usize {
        let n = core::cmp::min(out.len(), self.len);
        for b in out.iter_mut().take(n) {
            *b = self.buf[self.head];
            self.head = (self.head + 1) % RBUF;
        }
        self.len -= n;
        n
    }
}

#[derive(Clone, Copy)]
struct Pty {
    in_use: bool,
    master_closed: bool,
    slave_closed: bool,
    to_master: Ring, // shell output → terminal
    to_slave: Ring,  // cooked terminal input → shell
    canon: [u8; LINE],
    canon_len: usize,
    eof_pending: bool, // Ctrl-D on an empty line: next slave read returns 0 once
    iflag: u32,
    oflag: u32,
    lflag: u32,
    cc: [u8; NCCS],
    rows: u16,
    cols: u16,
    child: u32, // foreground pid (for future signal delivery)
    master_readers: WaitQ,
    slave_readers: WaitQ,
}

impl Pty {
    const fn new() -> Self {
        Pty {
            in_use: false,
            master_closed: false,
            slave_closed: false,
            to_master: Ring::new(),
            to_slave: Ring::new(),
            canon: [0; LINE],
            canon_len: 0,
            eof_pending: false,
            iflag: 0,
            oflag: 0,
            lflag: 0,
            cc: [0; NCCS],
            rows: 24,
            cols: 80,
            child: 0,
            master_readers: WaitQ::new(),
            slave_readers: WaitQ::new(),
        }
    }
    /// Cooked-mode defaults (what a freshly opened tty looks like).
    fn reset_termios(&mut self) {
        self.iflag = ICRNL;
        self.oflag = OPOST | ONLCR;
        self.lflag = ISIG | ICANON | ECHO | ECHOE | ECHOK;
        self.cc = [0; NCCS];
        self.cc[VINTR] = 0x03; // ^C
        self.cc[VQUIT] = 0x1c; // ^\
        self.cc[VERASE] = 0x7f; // DEL
        self.cc[VKILL] = 0x15; // ^U
        self.cc[VEOF] = 0x04; // ^D
    }
}

static PTYS: DiagMutex<[Pty; NPTYS]> = DiagMutex::new("PTYS", [Pty::new(); NPTYS]);

pub enum ReadOut {
    Data(usize),
    Eof,
    WouldBlock,
}

/// The fallout of feeding input through the line discipline: tids to wake on each
/// side, plus a signal to deliver to the fg child (sig 0 = none — async signal
/// delivery is a later step; the discipline already RECOGNIZES the key).
pub struct Ldisc {
    pub wake_master: [usize; QCAP],
    pub nwake_master: usize,
    pub wake_slave: [usize; QCAP],
    pub nwake_slave: usize,
    pub sig_pid: u32,
    pub sig: i32,
}

/// Allocate a fresh pty (cooked defaults). Returns its pool index, or None if full.
/// Resets fields IN PLACE (a `Pty` is ~8 KiB of rings — materializing one on the
/// kernel stack would overflow it, like the pipe pool).
pub fn create() -> Option<u8> {
    let mut ptys = PTYS.lock();
    for (i, p) in ptys.iter_mut().enumerate() {
        if !p.in_use {
            p.in_use = true;
            p.master_closed = false;
            p.slave_closed = false;
            p.to_master.head = 0;
            p.to_master.len = 0;
            p.to_slave.head = 0;
            p.to_slave.len = 0;
            p.canon_len = 0;
            p.eof_pending = false;
            p.rows = 24;
            p.cols = 80;
            p.child = 0;
            p.master_readers = WaitQ::new();
            p.slave_readers = WaitQ::new();
            p.reset_termios();
            return Some(i as u8);
        }
    }
    None
}

/// Master WRITE = bytes typed at the terminal. Runs the line discipline: echoes to
/// `to_master`, and (canonical) releases completed lines — or (raw) passes bytes —
/// to `to_slave`. Returns the wake/signal fallout for the syscall layer to apply.
pub fn master_write(idx: u8, src: &[u8]) -> Ldisc {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    let mut out = Ldisc {
        wake_master: [0; QCAP],
        nwake_master: 0,
        wake_slave: [0; QCAP],
        nwake_slave: 0,
        sig_pid: 0,
        sig: 0,
    };
    if !p.in_use {
        return out;
    }
    let mut echoed = false;
    let mut released = false;
    for &raw in src {
        let mut c = raw;
        // input CR/NL mapping
        if c == b'\r' && (p.iflag & ICRNL) != 0 {
            c = b'\n';
        } else if c == b'\n' && (p.iflag & INLCR) != 0 {
            c = b'\r';
        }
        // signals (cooked AND raw)
        if (p.lflag & ISIG) != 0 {
            if c == p.cc[VINTR] {
                p.canon_len = 0; // flush the partial line
                out.sig_pid = p.child;
                out.sig = 2; // SIGINT
                continue;
            }
            if c == p.cc[VQUIT] {
                out.sig_pid = p.child;
                out.sig = 3; // SIGQUIT
                continue;
            }
        }
        if (p.lflag & ICANON) != 0 {
            if c == p.cc[VERASE] {
                if p.canon_len > 0 {
                    p.canon_len -= 1;
                    if (p.lflag & ECHO) != 0 && (p.lflag & ECHOE) != 0 {
                        p.to_master.push(b"\x08 \x08"); // BS SP BS
                        echoed = true;
                    }
                }
                continue;
            }
            if c == p.cc[VKILL] {
                if (p.lflag & ECHO) != 0 && (p.lflag & ECHOK) != 0 {
                    while p.canon_len > 0 {
                        p.to_master.push(b"\x08 \x08");
                        p.canon_len -= 1;
                    }
                    echoed = true;
                } else {
                    p.canon_len = 0;
                }
                continue;
            }
            if c == p.cc[VEOF] {
                if p.canon_len == 0 {
                    p.eof_pending = true; // empty line: next slave read → EOF
                } else {
                    let n = p.canon_len;
                    let line = p.canon; // copy out before borrowing to_slave
                    p.to_slave.push(&line[..n]);
                    p.canon_len = 0;
                }
                released = true;
                continue;
            }
            if c == b'\n' {
                if p.canon_len < LINE {
                    p.canon[p.canon_len] = b'\n';
                    p.canon_len += 1;
                }
                let n = p.canon_len;
                let line = p.canon;
                p.to_slave.push(&line[..n]);
                p.canon_len = 0;
                released = true;
                if (p.lflag & ECHO) != 0 || (p.lflag & ECHONL) != 0 {
                    p.to_master.push(b"\r\n");
                    echoed = true;
                }
                continue;
            }
            // ordinary char
            if p.canon_len < LINE - 1 {
                p.canon[p.canon_len] = c;
                p.canon_len += 1;
                if (p.lflag & ECHO) != 0 {
                    p.to_master.push(&[c]);
                    echoed = true;
                }
            }
        } else {
            // raw mode: byte straight to the shell; echo only if ECHO is set.
            p.to_slave.push(&[c]);
            released = true;
            if (p.lflag & ECHO) != 0 {
                p.to_master.push(&[c]);
                echoed = true;
            }
        }
    }
    if echoed {
        out.nwake_master = p.master_readers.drain(&mut out.wake_master);
    }
    if released {
        out.nwake_slave = p.slave_readers.drain(&mut out.wake_slave);
    }
    out
}

/// Master READ = the terminal draining shell output. EOF once the slave side closes.
pub fn master_read(idx: u8, out: &mut [u8]) -> ReadOut {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    if !p.in_use {
        return ReadOut::Eof;
    }
    if p.to_master.len == 0 {
        if p.slave_closed {
            return ReadOut::Eof;
        }
        return ReadOut::WouldBlock;
    }
    ReadOut::Data(p.to_master.pop(out))
}

/// Slave WRITE = shell output. OPOST/ONLCR maps NL→CRNL into `to_master`; returns the
/// master-reader tids to wake.
pub fn slave_write(idx: u8, src: &[u8], wake: &mut [usize; QCAP]) -> usize {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    if !p.in_use {
        return 0;
    }
    if (p.oflag & OPOST) != 0 && (p.oflag & ONLCR) != 0 {
        for &b in src {
            if b == b'\n' {
                p.to_master.push(b"\r\n");
            } else {
                p.to_master.push(&[b]);
            }
        }
    } else {
        p.to_master.push(src);
    }
    p.master_readers.drain(wake)
}

/// Slave READ = the shell reading input. In canonical mode `to_slave` holds whole
/// lines only; a one-shot Ctrl-D (eof_pending) reports EOF. EOF also once the master
/// closes and nothing is buffered.
pub fn slave_read(idx: u8, out: &mut [u8]) -> ReadOut {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    if !p.in_use {
        return ReadOut::Eof;
    }
    if p.to_slave.len == 0 {
        if p.eof_pending {
            p.eof_pending = false;
            return ReadOut::Eof;
        }
        if p.master_closed {
            return ReadOut::Eof;
        }
        return ReadOut::WouldBlock;
    }
    ReadOut::Data(p.to_slave.pop(out))
}

/// Bytes available to read on the master (for poll readiness).
pub fn master_can_read(idx: u8) -> bool {
    let ptys = PTYS.lock();
    let p = &ptys[idx as usize];
    p.in_use && (p.to_master.len > 0 || p.slave_closed)
}

/// Bytes/line available to read on the slave (for poll readiness).
pub fn slave_can_read(idx: u8) -> bool {
    let ptys = PTYS.lock();
    let p = &ptys[idx as usize];
    p.in_use && (p.to_slave.len > 0 || p.eof_pending || p.master_closed)
}

/// Set the pty's foreground pid — the target for tty-generated signals (^C/^\).
/// Maintained by the kernel spawn/exit hooks (job-control-lite, §102): the newest
/// process spawned under this controlling tty is fg; on its exit, fg reverts to the
/// process it displaced.
pub fn set_fg(idx: u8, pid: u32) {
    PTYS.lock()[idx as usize].child = pid;
}

/// The pty's current foreground pid (0 = none).
pub fn fg_pid(idx: u8) -> u32 {
    PTYS.lock()[idx as usize].child
}

/// Copy out the termios flags + c_cc (for TCGETS).
pub fn get_termios(idx: u8) -> (u32, u32, u32, [u8; NCCS]) {
    let ptys = PTYS.lock();
    let p = &ptys[idx as usize];
    (p.iflag, p.oflag, p.lflag, p.cc)
}

/// Apply termios flags + c_cc (for TCSETS).
pub fn set_termios(idx: u8, iflag: u32, oflag: u32, lflag: u32, cc: [u8; NCCS]) {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    p.iflag = iflag;
    p.oflag = oflag;
    p.lflag = lflag;
    p.cc = cc;
}

pub fn get_winsize(idx: u8) -> (u16, u16) {
    let ptys = PTYS.lock();
    let p = &ptys[idx as usize];
    (p.rows, p.cols)
}

pub fn set_winsize(idx: u8, rows: u16, cols: u16) {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    p.rows = rows;
    p.cols = cols;
}

/// Close the master end; wake slave readers (they observe EOF).
pub fn close_master(idx: u8, wake: &mut [usize; QCAP]) -> usize {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    p.master_closed = true;
    let n = p.slave_readers.drain(wake);
    if p.slave_closed {
        p.in_use = false;
    }
    n
}

/// Close the slave end; wake master readers (they observe EOF).
pub fn close_slave(idx: u8, wake: &mut [usize; QCAP]) -> usize {
    let mut ptys = PTYS.lock();
    let p = &mut ptys[idx as usize];
    p.slave_closed = true;
    let n = p.master_readers.drain(wake);
    if p.master_closed {
        p.in_use = false;
    }
    n
}

pub fn park_master_reader(idx: u8, tid: usize) {
    PTYS.lock()[idx as usize].master_readers.push(tid);
}
pub fn park_slave_reader(idx: u8, tid: usize) {
    PTYS.lock()[idx as usize].slave_readers.push(tid);
}
pub fn unpark_master_reader(idx: u8, tid: usize) {
    PTYS.lock()[idx as usize].master_readers.remove(tid);
}
pub fn unpark_slave_reader(idx: u8, tid: usize) {
    PTYS.lock()[idx as usize].slave_readers.remove(tid);
}
