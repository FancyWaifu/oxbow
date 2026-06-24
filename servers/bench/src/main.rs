//! bench — microbenchmarks + exhaustion probes for oxbow. Run it from the shell
//! (`bench`); output goes to the console (and the serial mirror), so it can be read
//! headlessly. This fills oxbow's perf blind spot: nothing previously measured raw
//! syscall cost, IPC round-trip latency, or memory-map throughput, and nothing probed
//! how the allocator/scheduler behave at their limits.
//!
//! Methodology: fine-grained cost in CPU cycles via `rdtsc` (averaged over many
//! iterations, so rdtsc's non-serializing jitter is negligible); wall time via
//! `sys_uptime_ms` for throughput. Each phase warms up first so we measure steady
//! state, not first-touch faults. The exhaustion phases assert the kernel SURVIVES —
//! a graceful `E_NOMEM`/failed-spawn, never a panic (the spawn path was hardened for
//! exactly this).
#![no_std]
#![no_main]

use core::arch::x86_64::_rdtsc;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use oxbow_abi::{MsgBuf, BOOT_MEM, PROT_READ, PROT_WRITE};
use oxbow_rt as rt;

#[inline(always)]
fn tsc() -> u64 {
    // SAFETY: rdtsc is always available on x86_64 and readable from ring 3 (oxbow does
    // not set CR4.TSD). Non-serializing, but we amortize over N iterations.
    unsafe { _rdtsc() }
}

/// Sink so the optimizer can't elide the measured syscalls' results.
static SINK: AtomicU64 = AtomicU64::new(0);

// ---------------- Phase 1: raw syscall latency ----------------
fn bench_syscall() {
    const N: u64 = 200_000;
    for _ in 0..2000 {
        SINK.fetch_add(rt::sys_uptime_ms(), Relaxed); // warm up
    }
    let t0 = tsc();
    let mut acc = 0u64;
    for _ in 0..N {
        acc = acc.wrapping_add(rt::sys_uptime_ms());
    }
    let dt = tsc().wrapping_sub(t0);
    SINK.store(acc, Relaxed);
    rt::println!(
        "[bench] syscall  : {} cyc/call   (sys_uptime_ms x{})",
        dt / N,
        N
    );
}

// ---------------- Phase 2: IPC round-trip latency ----------------
// A responder thread blocks in sys_recv on a private endpoint and replies each call;
// main drives sys_call in a tight loop. Each iteration is a full round trip: user->
// kernel->block, schedule the responder, recv+reply, wake+return — the microkernel's
// hottest path (every fs read, every server request rides it).
static IPC_EP: AtomicU64 = AtomicU64::new(0);
static IPC_STOP: AtomicU32 = AtomicU32::new(0);
static mut IPC_STACK: [u8; 32 * 1024] = [0; 32 * 1024];

extern "C" fn ipc_responder(_: u64) -> ! {
    let ep = IPC_EP.load(Acquire) as u32;
    let mut m = MsgBuf::new(0);
    loop {
        if IPC_STOP.load(Acquire) != 0 {
            break;
        }
        if let Ok(h) = rt::sys_recv(ep, &mut m) {
            let _ = rt::sys_reply(h, &m);
        }
    }
    rt::sys_thread_exit()
}

fn bench_ipc() {
    let ep = match rt::sys_ep_create() {
        Ok(e) => e,
        Err(_) => {
            rt::println!("[bench] ipc      : SKIP (no endpoint)");
            return;
        }
    };
    IPC_EP.store(ep as u64, Release);
    let tid = unsafe { rt::spawn_thread(&mut *core::ptr::addr_of_mut!(IPC_STACK), ipc_responder, 0) };
    if tid == 0 {
        rt::println!("[bench] ipc      : SKIP (responder thread spawn failed)");
        return;
    }
    // Let the responder reach its first sys_recv before we start timing.
    for _ in 0..50_000 {
        core::hint::spin_loop();
    }
    const N: u64 = 100_000;
    let mut m = MsgBuf::new(0);
    for _ in 0..2000 {
        let _ = rt::sys_call(ep, &mut m); // warm up
    }
    let w0 = rt::sys_uptime_ms();
    let t0 = tsc();
    for _ in 0..N {
        let _ = rt::sys_call(ep, &mut m);
    }
    let dt = tsc().wrapping_sub(t0);
    let wall = rt::sys_uptime_ms().wrapping_sub(w0).max(1);
    IPC_STOP.store(1, Release);
    let _ = rt::sys_call(ep, &mut m); // unblock the responder so it exits
    rt::println!(
        "[bench] ipc rtt  : {} cyc/call   ({} round trips in {} ms = {}k/s)",
        dt / N,
        N,
        wall,
        N / wall
    );
}

// ---------------- Phase 3: memory-map throughput + graceful exhaustion ----------------
// Map fresh 4 KiB RW pages until the budget/PMM is exhausted. Measures map throughput
// AND that exhaustion returns E_NOMEM (loop ends) rather than panicking the kernel.
fn bench_mmap() {
    const STEP: u64 = 4096;
    const CAP: u64 = 1_000_000; // hard safety cap (never reached: budget bounds us first)
    let base = 0x4000_0000u64;
    let w0 = rt::sys_uptime_ms();
    let mut mapped = 0u64;
    while mapped < CAP {
        let va = base + mapped * STEP;
        match rt::sys_map(BOOT_MEM, va, STEP, PROT_READ | PROT_WRITE) {
            Ok(_) => mapped += 1,
            Err(_) => break, // graceful exhaustion — the point of the test
        }
    }
    let wall = rt::sys_uptime_ms().wrapping_sub(w0).max(1);
    let kib = mapped * 4;
    rt::println!(
        "[bench] mmap     : {} pages ({} KiB) in {} ms = {} MiB/s, then graceful E_NOMEM",
        mapped,
        kib,
        wall,
        (kib / 1024) * 1000 / wall
    );
    rt::println!("[bench] mmap     : kernel SURVIVED budget exhaustion (no panic)");
}

// ---------------- Phase 4: thread spawn throughput ----------------
// Spawn many short-lived threads (each bumps a counter and exits) from a mapped stack
// arena and time how long until all finish. Exercises the spawn path + per-thread TLS
// build + scheduler churn. A spawn that returns 0 (e.g. TCB pool full) ends the run
// gracefully — no panic — and we report how many we got.
const TN: u64 = 128;
const TSTK: u64 = 16 * 1024;
static TDONE: AtomicU32 = AtomicU32::new(0);

extern "C" fn exit_worker(_: u64) -> ! {
    TDONE.fetch_add(1, Release);
    rt::sys_thread_exit()
}

fn bench_thread_spawn() {
    let arena = 0x5000_0000u64;
    if rt::sys_map(BOOT_MEM, arena, TN * TSTK, PROT_READ | PROT_WRITE).is_err() {
        rt::println!("[bench] thread   : SKIP (stack arena map failed)");
        return;
    }
    TDONE.store(0, Release);
    let w0 = rt::sys_uptime_ms();
    let t0 = tsc();
    let mut ok = 0u64;
    for i in 0..TN {
        // SAFETY: each thread gets its OWN 16 KiB slot in the arena (no overlap), so
        // concurrent workers never share a stack.
        let stack = unsafe {
            core::slice::from_raw_parts_mut((arena + i * TSTK) as *mut u8, TSTK as usize)
        };
        let tid = unsafe { rt::spawn_thread(stack, exit_worker, 0) };
        if tid == 0 {
            break; // graceful spawn failure (e.g. TCB pool full)
        }
        ok += 1;
    }
    while (TDONE.load(Acquire) as u64) < ok {
        core::hint::spin_loop();
    }
    let dt = tsc().wrapping_sub(t0);
    let wall = rt::sys_uptime_ms().wrapping_sub(w0).max(1);
    rt::println!(
        "[bench] thread   : {} spawn+exit in {} ms = {} cyc/thread",
        ok,
        wall,
        if ok > 0 { dt / ok } else { 0 }
    );
    if ok < TN {
        rt::println!("[bench] thread   : graceful spawn failure at {} (kernel survived)", ok);
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    rt::println!("[bench] oxbow microbenchmarks + exhaustion probes");
    bench_syscall();
    bench_ipc();
    bench_thread_spawn();
    bench_mmap(); // LAST: it deliberately exhausts the whole budget (terminal phase)
    rt::println!("[bench] DONE — kernel survived all phases");
    rt::sys_exit(0)
}
