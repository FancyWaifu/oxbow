//! Kernel threads + the round-robin scheduler.
//!
//! A thread is a kernel stack + register context in the single v0 address space
//! (no per-process CR3 this arc). A fixed pool of TCBs and 16 KiB static kernel
//! stacks (no heap, law L6). TCB 0 is the boot/idle thread.
//!
//! Scheduling is cooperative here (Phase 3: `yield_now`); the timer drives it in
//! Phase 4 (`preempt`). The kernel is never preemptible (IF=0 in all kernel
//! code) — preemption only lands at ring-3 boundaries and `sti; hlt` idle points
//! — so `CURRENT`/the TCB array are plain globals with no locking (D-T1).
use core::ptr::{addr_of, addr_of_mut};

use crate::arch::{
    context_switch, disable_interrupts, enable_interrupts, thread_trampoline, wait_for_interrupt,
};
use crate::println;

pub const MAX_THREADS: usize = 8;
const KSTACK_SIZE: usize = 16 * 1024;
/// The boot thread becomes the idle thread; it is never in the Ready set.
const IDLE: usize = 0;

#[repr(align(16))]
#[allow(dead_code)] // backing buffer; only its address is taken
struct KStack([u8; KSTACK_SIZE]);

static mut KSTACKS: [KStack; MAX_THREADS] = [const { KStack([0; KSTACK_SIZE]) }; MAX_THREADS];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Free,
    Ready,
    Running,
    Exited,
}

#[derive(Clone, Copy)]
struct Tcb {
    state: State,
    ctx_rsp: u64,   // saved stack pointer (the whole context lives on the stack)
    kstack_top: u64,
}

static mut TCBS: [Tcb; MAX_THREADS] = [Tcb {
    state: State::Free,
    ctx_rsp: 0,
    kstack_top: 0,
}; MAX_THREADS];

static mut CURRENT: usize = IDLE;

// --- TCB field accessors (all access to the static-mut pool funnels here) ---
fn state(s: usize) -> State {
    unsafe { (*addr_of!(TCBS[s])).state }
}
fn set_state(s: usize, st: State) {
    unsafe { (*addr_of_mut!(TCBS[s])).state = st }
}
fn ctx_rsp(s: usize) -> u64 {
    unsafe { (*addr_of!(TCBS[s])).ctx_rsp }
}
fn ctx_slot(s: usize) -> *mut u64 {
    unsafe { addr_of_mut!(TCBS[s].ctx_rsp) }
}
fn kstack_top(s: usize) -> u64 {
    unsafe { (*addr_of!(TCBS[s])).kstack_top }
}

/// The currently-running TCB slot.
pub fn current() -> usize {
    unsafe { *addr_of!(CURRENT) }
}

/// Mark the boot thread (slot 0) as the running idle thread.
pub fn init() {
    set_state(IDLE, State::Running);
    unsafe { *addr_of_mut!(CURRENT) = IDLE };
}

/// Build a fake initial stack frame so the first switch into this thread
/// "returns" into the trampoline with entry in r12 and args in r13/r14.
fn init_stack(slot: usize, entry: u64, arg1: u64, arg2: u64) -> (u64, u64) {
    let top = unsafe { addr_of!(KSTACKS[slot]) as u64 } + KSTACK_SIZE as u64;
    let mut sp = top;
    let mut push = |v: u64| {
        sp -= 8;
        unsafe { *(sp as *mut u64) = v };
    };
    push(thread_trampoline as *const () as u64); // return address
    push(0); // rbx
    push(0); // rbp
    push(entry); // r12 -> entry fn
    push(arg1); // r13 -> arg1
    push(arg2); // r14 -> arg2
    push(0); // r15
    (sp, top)
}

fn spawn(entry: u64, arg1: u64, arg2: u64) -> usize {
    for slot in 1..MAX_THREADS {
        if state(slot) == State::Free {
            let (ctx_rsp, kstack_top) = init_stack(slot, entry, arg1, arg2);
            unsafe {
                *addr_of_mut!(TCBS[slot]) = Tcb {
                    state: State::Ready,
                    ctx_rsp,
                    kstack_top,
                };
            }
            return slot;
        }
    }
    panic!("thread: out of TCB slots");
}

/// Spawn a kernel thread; returns its TCB slot.
pub fn spawn_kernel(entry: extern "C" fn(u64), arg: u64) -> usize {
    spawn(entry as *const () as u64, arg, 0)
}

/// Round-robin scan for the next Ready thread after CURRENT (never returns
/// CURRENT, never returns the idle thread unless it's explicitly Ready).
fn pick_next() -> Option<usize> {
    let cur = current();
    for off in 1..MAX_THREADS {
        let s = (cur + off) % MAX_THREADS;
        if state(s) == State::Ready {
            return Some(s);
        }
    }
    None
}

/// Save the current context and resume `next`. Caller sets the outgoing
/// thread's state first (Ready/Exited).
fn switch_to(next: usize) {
    let prev = current();
    if prev == next {
        return;
    }
    set_state(next, State::Running);
    unsafe { *addr_of_mut!(CURRENT) = next };
    // Point TSS.RSP0 + the syscall entry stack at the incoming thread's kernel
    // stack BEFORE the switch — safe because IF=0 throughout the kernel, so
    // nothing can trap from ring 3 between this update and the switch.
    crate::arch::set_kernel_stack(kstack_top(next));
    context_switch(ctx_slot(prev), ctx_rsp(next));
}

/// Cooperatively yield to the next Ready thread (no-op if none). Unused in this
/// arc (preemption replaced it) but kept as a scheduler primitive.
#[allow(dead_code)]
pub fn yield_now() {
    match pick_next() {
        Some(n) => {
            set_state(current(), State::Ready);
            switch_to(n);
        }
        None => {}
    }
}

/// Terminate the current thread and switch away forever.
pub fn exit_current() -> ! {
    set_state(current(), State::Exited);
    let next = pick_next().unwrap_or(IDLE);
    switch_to(next);
    unreachable!("exited thread resumed");
}

/// Called from the timer IRQ handler (IF=0). Rotate to the next Ready thread;
/// if none, keep running the current one. The preempted thread resumes through
/// the handler tail's `iretq`, back where it was interrupted.
pub fn preempt() {
    if let Some(n) = pick_next() {
        if current() != IDLE {
            set_state(current(), State::Ready);
        }
        switch_to(n);
    }
}

/// True if any non-idle thread is still Ready or Running.
fn any_active() -> bool {
    (1..MAX_THREADS).any(|s| matches!(state(s), State::Ready | State::Running))
}

/// A thread's parking point: enable interrupts for exactly one `hlt`, so the
/// timer can fire and preempt us, then mask again (the kernel stays IF=0).
fn park_one_tick() {
    enable_interrupts();
    wait_for_interrupt();
    disable_interrupts();
}

/// The idle thread (TCB 0) body — never returns. Parks for ticks; the handler
/// reschedules to any Ready thread. We resume here only when nothing else is
/// runnable. Announces quiescence once every other thread has exited.
pub fn run_idle() -> ! {
    let mut quiescent = false;
    loop {
        if any_active() {
            quiescent = false;
        } else if !quiescent {
            println!("[idle] system quiescent");
            quiescent = true;
        }
        park_one_tick();
    }
}

// --- The user process P1, as a schedulable thread -------------------------

extern "C" fn user_thread_entry(entry: u64, user_rsp: u64) {
    // Becomes ring 3 forever via iretq; never returns to the trampoline.
    crate::arch::enter_user(entry, user_rsp);
}

/// Spawn the user process P1 as a thread. It enters ring 3 (IF=1) the first time
/// it is scheduled, and the timer preempts it mid-userspace thereafter.
pub fn spawn_user(entry: u64, user_rsp: u64) -> usize {
    spawn(user_thread_entry as *const () as u64, entry, user_rsp)
}

// --- A witness kernel thread, showing concurrency alongside the user ------

extern "C" fn witness(_arg: u64) {
    for n in 1..=3 {
        for _ in 0..15 {
            park_one_tick();
        }
        println!("[W] alive {}", n);
    }
    println!("[thr] witness exited (tcb {})", current());
    exit_current();
}

/// Spawn the witness thread.
pub fn spawn_witness() -> usize {
    spawn_kernel(witness, 0)
}
