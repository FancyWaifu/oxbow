//! §96 thread + futex + thread-safe-allocator self-test. Spawns a worker thread
//! in this process's address space; BOTH the worker and main then hammer the
//! oxbow-rt slab (Vec alloc/fill/sum/drop) concurrently — proving the slab's new
//! spinlock makes concurrent allocation safe. The worker signals done via a futex
//! (main blocks on it), then thread-exits without killing the process.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use oxbow_rt as rt;

const N: u32 = 50_000;
static COUNTER: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static WORKER_SUM: AtomicU64 = AtomicU64::new(0);
static mut WORKER_STACK: [u8; 128 * 1024] = [0; 128 * 1024];

fn done_ptr() -> *const u32 {
    (&DONE as *const AtomicU32).cast::<u32>()
}

// Allocate, fill, sum and drop a small heap Vec — churns the slab's free list.
fn churn(i: u32) -> u64 {
    let mut v: Vec<u64> = Vec::with_capacity(8);
    for j in 0..8u64 {
        v.push((i as u64).wrapping_mul(j + 1));
    }
    v.iter().copied().sum()
}

extern "C" fn worker(_arg: u64) -> ! {
    for i in 0..N {
        WORKER_SUM.fetch_add(churn(i), Ordering::Relaxed);
        COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    DONE.store(1, Ordering::Release);
    unsafe { rt::sys_futex_wake(done_ptr(), 1) };
    rt::sys_thread_exit()
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    rt::println!("[thrtest] spawning a worker; BOTH threads will hammer the heap");
    let tid = unsafe { rt::spawn_thread(&mut *addr_of_mut!(WORKER_STACK), worker, 0) };
    rt::println!("[thrtest] worker tid = {}", tid);

    // Contend the slab from the main thread while the worker runs.
    let mut main_sum: u64 = 0;
    for i in 0..N {
        main_sum = main_sum.wrapping_add(churn(i));
    }

    // Join via the futex.
    while DONE.load(Ordering::Acquire) == 0 {
        unsafe { rt::sys_futex_wait(done_ptr(), 0) };
    }

    let c = COUNTER.load(Ordering::Acquire);
    let ws = WORKER_SUM.load(Ordering::Relaxed);
    rt::println!("[thrtest] COUNTER={} (want {})", c, N);
    rt::println!("[thrtest] main_sum={} worker_sum={} (must match)", main_sum, ws);
    if c == N && main_sum == ws {
        rt::println!("[thrtest] PASS: concurrent heap churn safe + threads + futex");
    } else {
        rt::println!("[thrtest] FAIL: corruption or mismatch");
    }
    rt::sys_exit(0)
}
