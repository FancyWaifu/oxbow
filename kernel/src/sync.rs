//! §78 — `DiagMutex`: a `spin::Mutex` wrapper for hunting SMP deadlocks. It names
//! the lock and records the CPU that holds it, and if a waiter spins for too long it
//! fires the deadlock watchdog (`crate::deadlock_report`), which stops every core and
//! prints the exact lock + holder + every core's rip. The guard it returns IS a
//! `spin::MutexGuard`, so call sites are unchanged.
//!
//! `holder` tracks the LAST CPU to acquire — which is the CURRENT holder whenever the
//! lock is held (you can't acquire a held lock), so it's exactly right for the
//! deadlock report. It is intentionally not reset on unlock (would need a custom
//! guard); a stale value only matters for a FREE lock, which nobody is stuck on.
use core::sync::atomic::{AtomicI32, Ordering};
use spin::{Mutex, MutexGuard};

/// ~2 s of `pause` spins — far longer than any legitimate kernel critical section,
/// so this only ever fires on a real deadlock (no false positives).
const SPIN_DEADLOCK_LIMIT: u64 = 800_000_000;

pub struct DiagMutex<T> {
    holder: AtomicI32,
    name: &'static str,
    inner: Mutex<T>,
}

impl<T> DiagMutex<T> {
    pub const fn new(name: &'static str, val: T) -> Self {
        DiagMutex {
            holder: AtomicI32::new(-1),
            name,
            inner: Mutex::new(val),
        }
    }

    /// Force-unlock the inner mutex (for the panic console bypass). Same contract
    /// as `spin::Mutex::force_unlock`.
    ///
    /// # Safety
    /// Caller must ensure no live guard exists (e.g. the other cores are stopped).
    pub unsafe fn force_unlock(&self) {
        self.inner.force_unlock();
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        // Before per-CPU state exists (early BSP boot), `cpu_index()` (gs:[0]) would
        // fault and there's no SMP contention yet — so just take the lock plainly.
        if !crate::percpu::ready() {
            return self.inner.lock();
        }
        let mut spins: u64 = 0;
        loop {
            if let Some(g) = self.inner.try_lock() {
                self.holder
                    .store(crate::percpu::cpu_index() as i32, Ordering::Relaxed);
                return g;
            }
            core::hint::spin_loop();
            spins += 1;
            if spins > SPIN_DEADLOCK_LIMIT {
                crate::deadlock_report(
                    self.name,
                    crate::percpu::cpu_index() as i32,
                    self.holder.load(Ordering::Relaxed),
                );
            }
        }
    }
}
