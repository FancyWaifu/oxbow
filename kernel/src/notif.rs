//! Notification objects — the async signal primitive (a counting, latching
//! semaphore). Unlike IPC, `signal` is callable from an IRQ handler: it never
//! blocks and never switches. A driver `wait`s on it (blocking) until signalled.
//!
//! IRQ-context safety: the kernel is non-preemptible (IF=0 in all kernel code),
//! so an interrupt can only arrive from ring 3 or idle `sti;hlt` — never while a
//! kernel lock is held. That makes taking the pool lock inside the handler safe,
//! and makes publish-waiter→block atomic (no lost wakeup), the same argument
//! `block_current` already relies on.
use core::sync::atomic::{AtomicU8, Ordering};
use oxbow_abi::SysError;
use crate::sync::DiagMutex;

use crate::{ipc, thread};

// NOTE: pooled notifications are not freed when their last handle closes (the
// handle table just drops the entry — there is no object refcount yet), so a
// program that creates one per operation leaks pool slots. The shell therefore
// reuses a fixed set; this size gives boot servers + that set comfortable room.
const POOL_SIZE: usize = 24;

/// The notification the timer handler signals (a free ~1 Hz user clock, and the
/// permanent proof that IRQ-context signalling works). `NO_TICK` = unarmed.
const NO_TICK: u8 = 0xFF;
static TICK: AtomicU8 = AtomicU8::new(NO_TICK);

/// Arm the tick notification (called once at boot).
pub fn arm_tick(idx: u8) {
    TICK.store(idx, Ordering::Relaxed);
}

/// Signal the tick notification, if armed. Called from the timer IRQ handler.
pub fn fire_tick() {
    let t = TICK.load(Ordering::Relaxed);
    if t != NO_TICK {
        signal(t);
    }
}

#[derive(Clone, Copy)]
struct Notif {
    in_use: bool,
    count: u64,
    waiter: Option<usize>, // tid of the (single) blocked waiter
    /// tid of a bound receiver blocked in `sys_recv_notif`. Woken WITHOUT a deposited
    /// return (like the timer wake), so a concurrent endpoint handoff to the same
    /// thread can't be corrupted; the count stays latched if the sender wins the race.
    bound_waiter: Option<usize>,
    /// Last exit code delivered by `signal_exit` (an exit notification, §81). Read
    /// with `status` after waiting, so a shell can do `cmd1 && cmd2`. 0 otherwise.
    exit_code: i32,
}

static POOL: DiagMutex<[Notif; POOL_SIZE]> = DiagMutex::new("POOL",
    [Notif {
        in_use: false,
        count: 0,
        waiter: None,
        bound_waiter: None,
        exit_code: 0,
    }; POOL_SIZE],
);

/// Allocate a fresh notification; returns its pool index.
pub fn create() -> Option<u8> {
    let mut p = POOL.lock();
    for i in 0..POOL_SIZE {
        if !p[i].in_use {
            p[i] = Notif {
                in_use: true,
                count: 0,
                waiter: None,
                bound_waiter: None,
                exit_code: 0,
            };
            return Some(i as u8);
        }
    }
    None
}

/// Arm a bound receiver (`sys_recv_notif`): if the count is already latched, return
/// `false` (the caller returns "notif fired" at once without blocking); else register
/// `tid` so a later `signal` wakes it. The caller has ALREADY `prepare_block`ed, so a
/// signal between that and here is caught by the count re-check (returns false).
pub fn arm_bound(idx: u8, tid: usize) -> bool {
    let mut p = POOL.lock();
    let n = &mut p[idx as usize];
    if n.count > 0 {
        return false; // already signalled — don't block
    }
    n.bound_waiter = Some(tid);
    true
}

/// Clear `tid` as the bound receiver (on resume, if a message woke us instead).
pub fn clear_bound(idx: u8, tid: usize) {
    let mut p = POOL.lock();
    if p[idx as usize].bound_waiter == Some(tid) {
        p[idx as usize].bound_waiter = None;
    }
}

/// Drain the latched count to 0 (the bound receiver consumed the wake). Returns the
/// count that was latched.
pub fn drain(idx: u8) -> u64 {
    core::mem::take(&mut POOL.lock()[idx as usize].count)
}

/// Signal: bump the latched count, and hand it to a waiter if one is parked.
/// Safe from IRQ context — wake() only flips Blocked→Ready, no switch.
pub fn signal(idx: u8) {
    let mut p = POOL.lock();
    let n = &mut p[idx as usize];
    n.count = n.count.saturating_add(1);
    // A bound receiver (sys_recv_notif) is woken WITHOUT a deposited return — the count
    // stays latched and it drains it on resume. This asymmetry (a sender deposits, the
    // notif does not) is what makes a concurrent sender-vs-notif wake of the same thread
    // race-free: only the sender ever writes the thread's return slot.
    if let Some(tid) = n.bound_waiter.take() {
        drop(p);
        thread::wake(tid);
        return;
    }
    if let Some(tid) = n.waiter.take() {
        let c = core::mem::take(&mut n.count);
        drop(p);
        ipc::deposit_ret(tid, 0, c); // rax = OK, rdx = count
        thread::wake(tid);
    }
}

/// Signal AND record an exit code (§81): like `signal`, but stores `code` so a
/// waiter can read it via `status` after waking. Called from `proc::kill` so a
/// shell can branch on a child's exit status (`cmd1 && cmd2`).
pub fn signal_exit(idx: u8, code: i32) {
    {
        let mut p = POOL.lock();
        p[idx as usize].exit_code = code;
    }
    signal(idx);
}

/// The last exit code recorded on a notification (0 if none). Non-blocking; a
/// shell reads it right after `wait` returns for the child it spawned.
pub fn status(idx: u8) -> i32 {
    POOL.lock()[idx as usize].exit_code
}

/// Wait: drain a latched count immediately, else block until signalled. Returns
/// `(rax, rdx=count)`. One waiter per notification (a second → `E_NOMEM`).
pub fn wait(idx: u8) -> (u64, u64) {
    let me = thread::current();
    let mut p = POOL.lock();
    let n = &mut p[idx as usize];
    if n.count > 0 {
        return (0, core::mem::take(&mut n.count)); // latched — return at once
    }
    if n.waiter.is_some() {
        return (SysError::NoMem as u64, 0);
    }
    n.waiter = Some(me);
    // §70: commit to Blocked while still holding POOL (the interlock), so a
    // `signal()` on another CPU — which deposits our return then `wake`s us under
    // POOL — can't land between here and the sleep and be lost.
    thread::prepare_block();
    drop(p); // never hold the lock across block_current
    thread::block_current(); // sleep only if still Blocked; woken by signal()
    ipc::take_ret(me)
}

/// Non-blocking drain: take and return the latched count (0 if none), never
/// blocking. A driver polls this from a loop it can't park (the gpu's present
/// loop), to learn an async IRQ fired (a virtio-gpu config-change) without
/// stalling on `wait`.
pub fn poll(idx: u8) -> u64 {
    let mut p = POOL.lock();
    core::mem::take(&mut p[idx as usize].count)
}

/// Clear `tid` from any waiter slot (on thread exit), so a later signal can't
/// wake an Exited thread.
pub fn clear_waiter(tid: usize) {
    let mut p = POOL.lock();
    for n in p.iter_mut() {
        if n.waiter == Some(tid) {
            n.waiter = None;
        }
        if n.bound_waiter == Some(tid) {
            n.bound_waiter = None;
        }
    }
}
