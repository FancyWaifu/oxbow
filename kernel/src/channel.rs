//! channel — a bidirectional, byte-stream connection between two processes that
//! can ALSO carry capabilities inline (the socketpair + SCM_RIGHTS primitive,
//! §40). This is the transport a local protocol like Wayland runs over: the two
//! ends are `Channel` handles (one per process); each end can stream bytes and
//! attach capabilities that surface in the peer's handle table on receive —
//! which is exactly how Wayland passes shm-buffer fds, except an fd here is a
//! capability.
//!
//! A connection holds two directions; endpoint `side` 0 writes dir[0]/reads
//! dir[1], side 1 the reverse. Each direction is a ring buffer plus a small FIFO
//! of pending capabilities and reader/writer wait queues. Like `pipe`, this
//! module owns only buffer + queue bookkeeping under one lock (never held across
//! a block); the syscall layer drives block/retry with thread::block_current +
//! thread::wake.
use crate::object::HandleEntry;
use spin::Mutex;

const NCONN: usize = 8;
const CBUF: usize = 8192; // bytes per direction
const CAPQ: usize = 16; // pending caps per direction
const QCAP: usize = 8; // blocked tids per queue

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
    /// Remove a specific `tid` (compaction). Used to deregister a thread that
    /// parked on several channels at once (sys_chan_wait) when one wakes it.
    fn remove(&mut self, tid: usize) {
        let mut i = 0;
        while i < self.n {
            if self.q[i] == tid {
                self.q[i] = self.q[self.n - 1];
                self.n -= 1;
            } else {
                i += 1;
            }
        }
    }
}

/// One direction of a connection: a byte ring + a capability FIFO.
#[derive(Clone, Copy)]
struct Dir {
    buf: [u8; CBUF],
    head: usize,
    len: usize,
    caps: [Option<HandleEntry>; CAPQ],
    chead: usize,
    clen: usize,
    readers: WaitQ, // parked on recv of THIS direction
    writers: WaitQ, // parked on send into THIS direction (full)
}
impl Dir {
    const fn new() -> Self {
        Dir {
            buf: [0; CBUF],
            head: 0,
            len: 0,
            caps: [None; CAPQ],
            chead: 0,
            clen: 0,
            readers: WaitQ::new(),
            writers: WaitQ::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct Conn {
    in_use: bool,
    open: [bool; 2],
    dir: [Dir; 2],
}
impl Conn {
    const fn new() -> Self {
        Conn { in_use: false, open: [false; 2], dir: [Dir::new(), Dir::new()] }
    }
}

static CONNS: Mutex<[Conn; NCONN]> = Mutex::new([Conn::new(); NCONN]);

/// Reset a direction's bookkeeping in place (NOT the 8 KiB buffer — leaving it
/// uncleared is safe since `len`/`clen` go to 0). Done in place to avoid building
/// a multi-KiB `Dir`/`Conn` temporary on the small kernel stack (a by-value
/// `*c = Conn::new()` overflowed it).
fn reset_dir(d: &mut Dir) {
    d.head = 0;
    d.len = 0;
    d.chead = 0;
    d.clen = 0;
    d.caps = [None; CAPQ];
    d.readers = WaitQ::new();
    d.writers = WaitQ::new();
}

/// Allocate a fresh connected pair; returns its connection index (both sides
/// open) or None if the pool is full.
pub fn create() -> Option<u8> {
    let mut conns = CONNS.lock();
    for (i, c) in conns.iter_mut().enumerate() {
        if !c.in_use {
            reset_dir(&mut c.dir[0]);
            reset_dir(&mut c.dir[1]);
            c.in_use = true;
            c.open = [true, true];
            return Some(i as u8);
        }
    }
    None
}

/// Outcome of a non-blocking receive.
pub enum RecvOut {
    /// Bytes copied (may be 0 with caps); plus number of caps delivered.
    Data(usize),
    /// Peer closed and nothing left to read.
    Eof,
    /// Nothing buffered right now.
    WouldBlock,
}

#[inline]
fn read_dir(side: u8) -> usize {
    (1 - side) as usize // recv(side) reads the OTHER side's write direction
}

/// Try to receive on `side`: drain up to `out.len()` bytes and up to
/// `caps_out.len()` pending caps. Returns the outcome + #caps + writer tids to
/// wake (space freed). Caps are returned as HandleEntry values for the caller to
/// install into the receiver's table.
pub fn try_recv(
    idx: u8,
    side: u8,
    out: &mut [u8],
    caps_out: &mut [HandleEntry],
    wake: &mut [usize; QCAP],
) -> (RecvOut, usize, usize) {
    let mut conns = CONNS.lock();
    let c = &mut conns[idx as usize];
    if !c.in_use {
        return (RecvOut::Eof, 0, 0);
    }
    let peer_open = c.open[side as usize ^ 1];
    let d = &mut c.dir[read_dir(side)];
    if d.len == 0 && d.clen == 0 {
        if !peer_open {
            return (RecvOut::Eof, 0, 0);
        }
        return (RecvOut::WouldBlock, 0, 0);
    }
    let n = core::cmp::min(out.len(), d.len);
    for b in out.iter_mut().take(n) {
        *b = d.buf[d.head];
        d.head = (d.head + 1) % CBUF;
    }
    d.len -= n;
    // Deliver caps (FIFO), bounded by caller's buffer.
    let mut nc = 0;
    while nc < caps_out.len() && d.clen > 0 {
        if let Some(e) = d.caps[d.chead].take() {
            caps_out[nc] = e;
            nc += 1;
        }
        d.chead = (d.chead + 1) % CAPQ;
        d.clen -= 1;
    }
    let nwake = if n > 0 { d.writers.drain(wake) } else { 0 };
    (RecvOut::Data(n), nc, nwake)
}

/// Try to send on `side`: append bytes + attach caps into the send direction.
/// Returns (bytes written, whether caps were accepted, reader tids to wake). A
/// write of 0 with more to send means the buffer is full (block + retry). Caps
/// are only attached on the first call that makes progress, and only if the cap
/// FIFO has room; `caps_taken` reports how many were enqueued.
pub fn try_send(
    idx: u8,
    side: u8,
    src: &[u8],
    caps: &[HandleEntry],
    wake: &mut [usize; QCAP],
) -> (usize, usize, usize) {
    let mut conns = CONNS.lock();
    let c = &mut conns[idx as usize];
    if !c.in_use || !c.open[side as usize ^ 1] {
        return (0, 0, 0); // peer gone: report 0 (caller maps to EPIPE)
    }
    let d = &mut c.dir[side as usize];
    let space = CBUF - d.len;
    let n = core::cmp::min(src.len(), space);
    for &b in src.iter().take(n) {
        let pos = (d.head + d.len) % CBUF;
        d.buf[pos] = b;
        d.len += 1;
    }
    // Attach caps once there is room in the cap FIFO.
    let mut taken = 0;
    while taken < caps.len() && d.clen < CAPQ {
        let pos = (d.chead + d.clen) % CAPQ;
        d.caps[pos] = Some(caps[taken]);
        d.clen += 1;
        taken += 1;
    }
    let nwake = if n > 0 || taken > 0 { d.readers.drain(wake) } else { 0 };
    (n, taken, nwake)
}

/// Non-destructive readiness for `side` (for epoll/poll): bit0 readable (data,
/// caps, or peer-closed-EOF pending), bit1 EOF (peer closed), bit2 writable
/// (send buffer has room and the peer is open).
pub fn poll(idx: u8, side: u8) -> u64 {
    let conns = CONNS.lock();
    let c = &conns[idx as usize];
    if !c.in_use {
        return 0b011; // gone: readable + EOF
    }
    let peer_open = c.open[side as usize ^ 1];
    let rd = &c.dir[read_dir(side)];
    let wr = &c.dir[side as usize];
    let mut bits = 0u64;
    if rd.len > 0 || rd.clen > 0 || !peer_open {
        bits |= 0b001; // readable (or EOF-readable)
    }
    if !peer_open {
        bits |= 0b010; // EOF
    }
    if peer_open && wr.len < CBUF {
        bits |= 0b100; // writable
    }
    bits
}

/// Whether the peer of `side` is still open (false => sends should fail EPIPE).
pub fn peer_open(idx: u8, side: u8) -> bool {
    let conns = CONNS.lock();
    let c = &conns[idx as usize];
    c.in_use && c.open[side as usize ^ 1]
}

/// Park `tid` waiting to receive on `side` (woken when the peer sends).
pub fn park_recv(idx: u8, side: u8, tid: usize) {
    CONNS.lock()[idx as usize].dir[read_dir(side)].readers.push(tid);
}

/// Park `tid` waiting to send on `side` (woken when the peer drains).
pub fn park_send(idx: u8, side: u8, tid: usize) {
    CONNS.lock()[idx as usize].dir[side as usize].writers.push(tid);
}

/// Deregister `tid` from `side`'s reader queue — the counterpart to `park_recv`,
/// used by sys_chan_wait to clean up after parking on several channels at once.
pub fn unpark_recv(idx: u8, side: u8, tid: usize) {
    CONNS.lock()[idx as usize].dir[read_dir(side)].readers.remove(tid);
}

/// Close `side`; returns peer tids to wake (they observe EOF). Frees the
/// connection once both sides are closed.
pub fn close(idx: u8, side: u8, wake: &mut [usize; QCAP]) -> usize {
    let mut conns = CONNS.lock();
    let c = &mut conns[idx as usize];
    if !c.in_use {
        return 0;
    }
    c.open[side as usize] = false;
    // Wake anyone blocked reading the now-dead direction (the peer reads dir[side]).
    let nwake = c.dir[side as usize].readers.drain(wake);
    if !c.open[0] && !c.open[1] {
        c.in_use = false; // free in place (no multi-KiB stack temporary)
    }
    nwake
}
