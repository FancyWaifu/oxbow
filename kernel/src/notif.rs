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
use spin::Mutex;

use crate::{ipc, thread};

const POOL_SIZE: usize = 8;

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
}

static POOL: Mutex<[Notif; POOL_SIZE]> = Mutex::new(
    [Notif {
        in_use: false,
        count: 0,
        waiter: None,
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
            };
            return Some(i as u8);
        }
    }
    None
}

/// Signal: bump the latched count, and hand it to a waiter if one is parked.
/// Safe from IRQ context — wake() only flips Blocked→Ready, no switch.
pub fn signal(idx: u8) {
    let mut p = POOL.lock();
    let n = &mut p[idx as usize];
    n.count = n.count.saturating_add(1);
    if let Some(tid) = n.waiter.take() {
        let c = core::mem::take(&mut n.count);
        drop(p);
        ipc::deposit_ret(tid, 0, c); // rax = OK, rdx = count
        thread::wake(tid);
    }
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

/// Clear `tid` from any waiter slot (on thread exit), so a later signal can't
/// wake an Exited thread.
pub fn clear_waiter(tid: usize) {
    let mut p = POOL.lock();
    for n in p.iter_mut() {
        if n.waiter == Some(tid) {
            n.waiter = None;
        }
    }
}
