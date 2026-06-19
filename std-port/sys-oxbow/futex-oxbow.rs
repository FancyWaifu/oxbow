//! oxbow futex — std's generic Mutex/Condvar/RwLock/Once/parking run on this,
//! backed by oxbow-rt's hosted shims over SYS_FUTEX_WAIT/WAKE.
#![cfg(target_os = "oxbow")]

use crate::sync::atomic::Atomic;
use crate::time::Duration;

pub type Futex = Atomic<Primitive>;
pub type Primitive = u32;
pub type SmallFutex = Atomic<SmallPrimitive>;
pub type SmallPrimitive = u32;

unsafe extern "C" {
    fn __oxbow_futex_wait(addr: *const u32, expected: u32);
    fn __oxbow_futex_wake(addr: *const u32) -> u32;
    fn __oxbow_futex_wake_all(addr: *const u32);
}

#[inline]
fn ptr(futex: &Atomic<u32>) -> *const u32 {
    futex as *const Atomic<u32> as *const u32
}

/// Waits while `*futex == expected`. Returns false on timeout — oxbow has no futex
/// timeout yet, so a `Some(timeout)` currently blocks until woken (TODO: timed wait).
pub fn futex_wait(futex: &Atomic<u32>, expected: u32, _timeout: Option<Duration>) -> bool {
    unsafe { __oxbow_futex_wait(ptr(futex), expected) };
    true
}

pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    unsafe { __oxbow_futex_wake(ptr(futex)) > 0 }
}

pub fn futex_wake_all(futex: &Atomic<u32>) {
    unsafe { __oxbow_futex_wake_all(ptr(futex)) };
}
