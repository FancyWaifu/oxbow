//! §96 thread + futex self-test. Spawns a second thread in this process's address
//! space (SYS_THREAD_SPAWN), has it do work and signal completion via a futex
//! (SYS_FUTEX_WAKE), while the main thread blocks on the futex (SYS_FUTEX_WAIT),
//! then the worker exits its thread (SYS_THREAD_EXIT) without killing the process.
//!
//! The worker does pure-atomic work and NO heap/println: the oxbow-rt slab is
//! single-threaded (no CAS), so concurrent allocation would race. The main thread
//! owns all printing (before the spawn and after the join).
#![no_std]
#![no_main]

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};
use oxbow_rt as rt;

const N: u32 = 200_000;
static COUNTER: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static mut WORKER_STACK: [u8; 64 * 1024] = [0; 64 * 1024];

fn done_ptr() -> *const u32 {
    (&DONE as *const AtomicU32).cast::<u32>()
}

extern "C" fn worker(_arg: u64) -> ! {
    for _ in 0..N {
        COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    DONE.store(1, Ordering::Release);
    unsafe { rt::sys_futex_wake(done_ptr(), 1) };
    rt::sys_thread_exit()
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    rt::println!("[thrtest] spawning a worker thread in this address space");
    let tid = unsafe { rt::spawn_thread(&mut *addr_of_mut!(WORKER_STACK), worker, 0) };
    rt::println!("[thrtest] worker tid = {}; futex-waiting for it", tid);
    while DONE.load(Ordering::Acquire) == 0 {
        unsafe { rt::sys_futex_wait(done_ptr(), 0) };
    }
    let c = COUNTER.load(Ordering::Acquire);
    rt::println!("[thrtest] worker finished: COUNTER = {} (expected {})", c, N);
    if c == N {
        rt::println!("[thrtest] PASS: SYS_THREAD_SPAWN + futex + SYS_THREAD_EXIT");
    } else {
        rt::println!("[thrtest] FAIL: counter mismatch");
    }
    rt::sys_exit(0)
}
