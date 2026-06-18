//! pipe — kernel-buffered unidirectional byte channels (§39), the primitive
//! behind shell pipelines. A pipe is a ring buffer with two wait queues: readers
//! block while it is empty (and get EOF once the write side closes), writers block
//! while it is full. The syscall layer (kernel/src/syscall.rs) drives the
//! block/retry loop with `thread::block_current` + `thread::wake`, mirroring the
//! endpoint rendezvous; this module owns only the buffer + queue bookkeeping under
//! a single lock (never held across a block).
use crate::sync::DiagMutex;

const NPIPES: usize = 8;
const PBUF: usize = 8192;
const QCAP: usize = 8;

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
    /// Empty the queue into `out`, returning the count to wake.
    fn drain(&mut self, out: &mut [usize; QCAP]) -> usize {
        let n = self.n;
        out[..n].copy_from_slice(&self.q[..n]);
        self.n = 0;
        n
    }
    /// Remove `tid` if present (swap-remove). Used when a parked waiter leaves the
    /// wait without sleeping (it found the condition true), so its slot is freed.
    fn remove(&mut self, tid: usize) {
        if let Some(pos) = self.q[..self.n].iter().position(|&t| t == tid) {
            self.n -= 1;
            self.q[pos] = self.q[self.n];
        }
    }
}

#[derive(Clone, Copy)]
struct Pipe {
    in_use: bool,
    buf: [u8; PBUF],
    head: usize, // read position
    len: usize,  // bytes currently buffered
    write_closed: bool,
    readers: WaitQ,
    writers: WaitQ,
}
impl Pipe {
    const fn new() -> Self {
        Pipe {
            in_use: false,
            buf: [0; PBUF],
            head: 0,
            len: 0,
            write_closed: false,
            readers: WaitQ::new(),
            writers: WaitQ::new(),
        }
    }
}

static PIPES: DiagMutex<[Pipe; NPIPES]> = DiagMutex::new("PIPES", [Pipe::new(); NPIPES]);

/// The outcome of a non-blocking read attempt.
pub enum ReadOut {
    Data(usize),
    Eof,
    WouldBlock,
}

/// Allocate a fresh pipe; returns its pool index or None if the pool is full.
/// Resets the slot's fields IN PLACE rather than `*p = Pipe::new()` — the latter
/// materializes a whole `Pipe` (an 8 KiB `buf`) as a temporary on the kernel
/// stack, which overflowed/clobbered the 32 KiB syscall stack and faulted on
/// return. `buf` needs no reset: only the live `[head, head+len)` window is read.
pub fn create() -> Option<u8> {
    let mut pipes = PIPES.lock();
    for (i, p) in pipes.iter_mut().enumerate() {
        if !p.in_use {
            p.in_use = true;
            p.head = 0;
            p.len = 0;
            p.write_closed = false;
            p.readers = WaitQ::new();
            p.writers = WaitQ::new();
            return Some(i as u8);
        }
    }
    None
}

/// Try to read up to `out.len()` bytes. On success, also returns the writer tids
/// to wake (space freed). `wake` holds the tids; `nwake` how many are valid.
pub fn try_read(idx: u8, out: &mut [u8], wake: &mut [usize; QCAP]) -> (ReadOut, usize) {
    let mut pipes = PIPES.lock();
    let p = &mut pipes[idx as usize];
    if !p.in_use {
        return (ReadOut::Eof, 0);
    }
    if p.len == 0 {
        if p.write_closed {
            return (ReadOut::Eof, 0);
        }
        return (ReadOut::WouldBlock, 0);
    }
    let n = core::cmp::min(out.len(), p.len);
    for b in out.iter_mut().take(n) {
        *b = p.buf[p.head];
        p.head = (p.head + 1) % PBUF;
    }
    p.len -= n;
    let nwake = p.writers.drain(wake);
    (ReadOut::Data(n), nwake)
}

/// Try to write `src`. Returns (bytes written, reader tids to wake). A return of
/// 0 written means the buffer was full (WouldBlock); the caller blocks + retries.
pub fn try_write(idx: u8, src: &[u8], wake: &mut [usize; QCAP]) -> (usize, usize) {
    let mut pipes = PIPES.lock();
    let p = &mut pipes[idx as usize];
    if !p.in_use {
        return (0, 0);
    }
    let space = PBUF - p.len;
    let n = core::cmp::min(src.len(), space);
    for &b in src.iter().take(n) {
        let pos = (p.head + p.len) % PBUF;
        p.buf[pos] = b;
        p.len += 1;
    }
    let nwake = if n > 0 { p.readers.drain(wake) } else { 0 };
    (n, nwake)
}

/// Mark the write side closed; returns reader tids to wake (they get EOF).
pub fn mark_eof(idx: u8, wake: &mut [usize; QCAP]) -> usize {
    let mut pipes = PIPES.lock();
    let p = &mut pipes[idx as usize];
    p.write_closed = true;
    p.readers.drain(wake)
}

/// Park `tid` on the read queue (it will be woken by a writer or by EOF).
pub fn park_reader(idx: u8, tid: usize) {
    PIPES.lock()[idx as usize].readers.push(tid);
}

/// Park `tid` on the write queue (woken when a reader frees space).
pub fn park_writer(idx: u8, tid: usize) {
    PIPES.lock()[idx as usize].writers.push(tid);
}

/// Remove `tid` from the read queue — it found data/EOF and isn't sleeping (§70).
pub fn unpark_reader(idx: u8, tid: usize) {
    PIPES.lock()[idx as usize].readers.remove(tid);
}

/// Remove `tid` from the write queue — it made progress and isn't sleeping (§70).
pub fn unpark_writer(idx: u8, tid: usize) {
    PIPES.lock()[idx as usize].writers.remove(tid);
}
