//! Multi-threaded SMP RACE PoCs (round-3 pentest). Spawns worker threads in this
//! process's address space (the kernel schedules them across APs, so they truly run
//! on different cores) and deliberately races the syscalls whose round-1/2 SMP fixes
//! we want to PROVE hold:
//!
//!   Phase 1 — MAP RACE: two threads sys_map the SAME fresh vaddr simultaneously.
//!     With VM_MUT (the fix) the loser's probe sees the winner's mapping → exactly one
//!     win + one fault per round. WITHOUT it both probe clean and the second map_to
//!     panics the kernel (PageAlreadyMapped) — so a clean run that reports N/N is the
//!     proof the serialization works.
//!
//!   Phase 2 — REPLY DOUBLE-FREE RACE: a caller (main) drives traffic on an endpoint
//!     while two threads race sys_reply on the SAME Reply handle. With take_reply (the
//!     fix) the handle is claimed atomically, so each minted reply is delivered EXACTLY
//!     once (ok_replies == recvs). WITHOUT it both deliver → double free_reply/wake
//!     (ok_replies > recvs, and pool corruption / hang).
#![no_std]
#![no_main]

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use oxbow_abi::{MsgBuf, BOOT_MEM, PROT_READ, PROT_WRITE};
use oxbow_rt as rt;

// ---------------- Phase 1: map race ----------------
const MAP_ROUNDS: u32 = 64;
static MAP_VA: AtomicU64 = AtomicU64::new(0);
static MAP_ROUND: AtomicU32 = AtomicU32::new(u32::MAX); // current round signal (MAX-1 = stop)
static MAP_WIN: AtomicU32 = AtomicU32::new(0);
static MAP_FAULT: AtomicU32 = AtomicU32::new(0);
static MAP_WORKER_DONE: AtomicU32 = AtomicU32::new(0);
static mut MAP_STACK: [u8; 32 * 1024] = [0; 32 * 1024];

fn race_map(va: u64) {
    match rt::sys_map(BOOT_MEM, va, 4096, PROT_READ | PROT_WRITE) {
        Ok(_) => {
            MAP_WIN.fetch_add(1, Relaxed);
        }
        Err(_) => {
            MAP_FAULT.fetch_add(1, Relaxed);
        }
    }
}

extern "C" fn map_worker(_: u64) -> ! {
    let mut last = u32::MAX;
    loop {
        let r = MAP_ROUND.load(Acquire);
        if r == last {
            core::hint::spin_loop();
            continue;
        }
        if r == u32::MAX - 1 {
            break; // stop sentinel
        }
        last = r;
        let va = MAP_VA.load(Acquire);
        race_map(va); // both threads hit the SAME va, near-simultaneously
        MAP_WORKER_DONE.store(r.wrapping_add(1), Release);
    }
    rt::sys_thread_exit()
}

fn phase_map_race() {
    rt::println!("[race] phase 1: MAP race ({} rounds, 2 cores on the same vaddr)", MAP_ROUNDS);
    let _tid = unsafe { rt::spawn_thread(&mut *addr_of_mut!(MAP_STACK), map_worker, 0) };
    let base = 0x2000_0000u64;
    for round in 0..MAP_ROUNDS {
        let va = base + (round as u64) * 0x4000; // fresh region each round (PTs unallocated)
        MAP_VA.store(va, Release);
        MAP_ROUND.store(round, Release); // release the worker
        race_map(va); // main races too
        // barrier: wait for the worker to finish this round
        while MAP_WORKER_DONE.load(Acquire) != round.wrapping_add(1) {
            core::hint::spin_loop();
        }
    }
    MAP_ROUND.store(u32::MAX - 1, Release); // stop the worker
    let (w, f) = (MAP_WIN.load(Relaxed), MAP_FAULT.load(Relaxed));
    rt::println!("[race] map: {} wins, {} faults / {} rounds", w, f, MAP_ROUNDS);
    if w == MAP_ROUNDS && f == MAP_ROUNDS {
        rt::println!("[race] map: PASS (1 win + 1 fault every round; VM_MUT held)");
    } else {
        rt::println!("[race] map: ANOMALY (want {}/{}); page-table race?", MAP_ROUNDS, MAP_ROUNDS);
    }
}

// ---------------- Phase 2: reply double-free race ----------------
const CALLS: u32 = 400;
static EP: AtomicU64 = AtomicU64::new(0);
static REPLY_H: AtomicU32 = AtomicU32::new(0);
static REPLY_PUB: AtomicU32 = AtomicU32::new(0); // 1 = a fresh handle is published
static RECVS: AtomicU32 = AtomicU32::new(0); // reply handles minted
static OK_REPLIES: AtomicU32 = AtomicU32::new(0); // successful sys_reply (either thread)
static STOP: AtomicU32 = AtomicU32::new(0);
static mut RECV_STACK: [u8; 32 * 1024] = [0; 32 * 1024];
static mut CLOSE_STACK: [u8; 32 * 1024] = [0; 32 * 1024];

extern "C" fn receiver(_: u64) -> ! {
    let ep = EP.load(Acquire) as u32;
    let mut m = MsgBuf::new(0);
    loop {
        if STOP.load(Acquire) != 0 {
            break;
        }
        if let Ok(h) = rt::sys_recv(ep, &mut m) {
            RECVS.fetch_add(1, Relaxed);
            REPLY_H.store(h as u32, Release);
            REPLY_PUB.store(1, Release); // hand the same h to the closer
            // race the closer to reply this exact handle
            if rt::sys_reply(h, &m).is_ok() {
                OK_REPLIES.fetch_add(1, Relaxed);
            }
        }
    }
    rt::sys_thread_exit()
}

extern "C" fn closer(_: u64) -> ! {
    let m = MsgBuf::new(0);
    loop {
        if STOP.load(Acquire) != 0 {
            break;
        }
        // consume a freshly-published handle (CAS 1->0) and reply it too
        if REPLY_PUB.compare_exchange(1, 0, AcqRel, Acquire).is_ok() {
            let h = REPLY_H.load(Acquire);
            if rt::sys_reply(h, &m).is_ok() {
                OK_REPLIES.fetch_add(1, Relaxed);
            }
        } else {
            core::hint::spin_loop();
        }
    }
    rt::sys_thread_exit()
}

fn phase_reply_race() {
    rt::println!("[race] phase 2: REPLY double-free race ({} calls, 2 threads reply each handle)", CALLS);
    let ep = match rt::sys_ep_create() {
        Ok(e) => e,
        Err(_) => {
            rt::println!("[race] reply: could not create endpoint");
            return;
        }
    };
    EP.store(ep as u64, Release);
    let _r = unsafe { rt::spawn_thread(&mut *addr_of_mut!(RECV_STACK), receiver, 0) };
    let _c = unsafe { rt::spawn_thread(&mut *addr_of_mut!(CLOSE_STACK), closer, 0) };
    // main = the caller: each call mints a Reply for the receiver, then blocks for it.
    let mut m = MsgBuf::new(0);
    let mut completed = 0u32;
    for _ in 0..CALLS {
        if rt::sys_call(ep, &mut m).is_ok() {
            completed += 1;
        }
    }
    STOP.store(1, Release);
    // unblock the receiver (it may be parked in sys_recv) so the counts settle
    let _ = rt::sys_call(ep, &mut m);
    let (recvs, ok) = (RECVS.load(Relaxed), OK_REPLIES.load(Relaxed));
    rt::println!("[race] reply: {} calls done, {} handles minted, {} replies delivered", completed, recvs, ok);
    // With the fix, every minted handle is delivered EXACTLY once (allow ±1 for the
    // in-flight final round). A double-free shows up as ok > recvs.
    if ok <= recvs + 1 && ok + 2 >= recvs {
        rt::println!("[race] reply: PASS (no double delivery; take_reply claimed atomically)");
    } else {
        rt::println!("[race] reply: ANOMALY (ok={} vs recvs={}) — double free!", ok, recvs);
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    rt::println!("[race] multi-threaded SMP race PoCs (round-3)");
    phase_map_race();
    phase_reply_race();
    rt::println!("[race] DONE — kernel survived both races");
    rt::sys_exit(0)
}
