//! oxbow keyed TLS — per-thread storage indexed by (tid, key). Each thread only
//! ever touches its own table row (TLS is per-thread), so the rows need no
//! locking; only key allocation is shared (an atomic counter). The current tid
//! comes from oxbow-rt's `__oxbow_thread_id` shim (SYS_THREAD_ID).
//!
//! No per-key destructors yet (oxbow's TLS guard is a no-op), so TLS values leak
//! at thread exit — acceptable for v1; the point is correctness (no sharing/UAF).
use crate::ptr;
use crate::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

const MAX_THREADS: usize = 256; // must cover the kernel's tid range (TCB pool)
const MAX_KEYS: usize = 32;

/// 1-based; `racy::LazyKey` reserves 0 (KEY_SENTVAL) for "uninitialized".
pub type Key = usize;

static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
static TABLE: [[AtomicPtr<u8>; MAX_KEYS]; MAX_THREADS] =
    [const { [const { AtomicPtr::new(ptr::null_mut()) }; MAX_KEYS] }; MAX_THREADS];

unsafe extern "C" {
    fn __oxbow_thread_id() -> u64;
}

#[inline]
fn tid() -> usize {
    let t = unsafe { __oxbow_thread_id() } as usize;
    debug_assert!(t < MAX_THREADS, "oxbow TLS: tid out of range");
    t
}

#[inline]
pub fn create(_dtor: Option<unsafe extern "C" fn(*mut u8)>) -> Key {
    let k = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    assert!(k < MAX_KEYS, "oxbow TLS: out of keys");
    k
}

#[inline]
pub unsafe fn set(key: Key, value: *mut u8) {
    TABLE[tid()][key].store(value, Ordering::Relaxed);
}

#[inline]
pub unsafe fn get(key: Key) -> *mut u8 {
    TABLE[tid()][key].load(Ordering::Relaxed)
}

#[inline]
pub unsafe fn destroy(_key: Key) {
    // Keys are never reclaimed.
}
