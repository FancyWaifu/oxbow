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
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{
    context_switch, disable_interrupts, enable_interrupts, thread_trampoline, wait_for_interrupt,
};
use crate::println;

/// Announce the first real CR3 reload (first user dispatch) as a checkpoint.
fn announce_first_cr3(proc: usize) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if !DONE.swap(true, Ordering::Relaxed) {
        println!("[sched] cr3 -> proc {} (first user dispatch)", proc);
    }
}

// Boot now brings up the desktop (oxcomp + oxterm + wlclient + sysmon) on top of
// the servers (shell, fs, net, tty, kbd, blk, …), which alone fill ~12 slots — so a
// user command like `ls` had no slot left ("out of TCB slots"). Give generous
// headroom for the desktop plus several concurrent user processes / pipelines. Each
// slot costs a 16 KiB kernel stack + a 512 B FX area (static BSS), so this is cheap.
pub const MAX_THREADS: usize = 32;
const KSTACK_SIZE: usize = 16 * 1024;
/// The boot thread becomes the idle thread; it is never in the Ready set.
const IDLE: usize = 0;

#[repr(align(16))]
#[allow(dead_code)] // backing buffer; only its address is taken
struct KStack([u8; KSTACK_SIZE]);

static mut KSTACKS: [KStack; MAX_THREADS] = [const { KStack([0; KSTACK_SIZE]) }; MAX_THREADS];

/// Per-thread x87/SSE save area (FXSAVE layout), 16-byte aligned. A thread that
/// uses SSE (the DRIFT crypto) keeps its XMM/MXCSR here across context switches.
#[repr(align(16))]
#[allow(dead_code)] // backing buffer; only its address is taken (fxsave/fxrstor)
struct FxArea([u8; crate::arch::FXSAVE_SIZE]);

static mut FX_AREAS: [FxArea; MAX_THREADS] =
    [const { FxArea([0; crate::arch::FXSAVE_SIZE]) }; MAX_THREADS];
/// A clean post-`fninit` save area, captured once at boot and copied into every
/// thread's area at init and on slot reuse (so a fresh thread starts with valid,
/// zeroed FPU state instead of whatever the previous occupant left).
static mut FX_TEMPLATE: FxArea = FxArea([0; crate::arch::FXSAVE_SIZE]);

fn fx_area_ptr(slot: usize) -> *mut u8 {
    unsafe { addr_of_mut!(FX_AREAS[slot]) as *mut u8 }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Free,
    Ready,
    Running,
    Blocked,
    Exited,
}

/// Sentinel for "no owning process" (kernel threads: idle, witness).
pub const NO_PROC: usize = usize::MAX;

#[derive(Clone, Copy)]
struct Tcb {
    state: State,
    ctx_rsp: u64,    // saved stack pointer (the whole context lives on the stack)
    kstack_top: u64,
    proc: usize,     // owning process id, or NO_PROC for kernel threads
    cr3: u64,        // address-space root to load on switch (0 = keep live CR3)
    /// Timer deadline in ticks (0 = none). When `TICKS >= wake_at`, the timer IRQ
    /// wakes this Blocked thread. Used for timed waits (sys_chan_wait timeout).
    wake_at: u64,
}

static mut TCBS: [Tcb; MAX_THREADS] = [Tcb {
    state: State::Free,
    ctx_rsp: 0,
    kstack_top: 0,
    proc: NO_PROC,
    cr3: 0,
    wake_at: 0,
}; MAX_THREADS];

// §69 Phase 4: the running thread is PER-CPU — it lives in this CPU's PerCpu
// (reached via the GS base), not a single global. `current()`/`set_running()`
// funnel through `crate::percpu`. On one core this is identical to the old global.

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
fn cr3_of(s: usize) -> u64 {
    unsafe { (*addr_of!(TCBS[s])).cr3 }
}
fn proc_of(s: usize) -> usize {
    unsafe { (*addr_of!(TCBS[s])).proc }
}

/// The TCB slot currently running on THIS CPU.
pub fn current() -> usize {
    crate::percpu::current()
}

/// The process owning the current thread (valid during a syscall — those only
/// come from user threads).
pub fn current_proc() -> usize {
    proc_of(current())
}

/// The process owning thread `tid` (for IPC peer resolution).
pub fn process_of(tid: usize) -> usize {
    proc_of(tid)
}

/// Mark the boot thread (slot 0) as the running idle thread.
pub fn init() {
    set_state(IDLE, State::Running);
    crate::percpu::set_current(IDLE);
    crate::percpu::set_idle_tid(IDLE); // the BSP idles on TCB 0
    // SSE was enabled + `fninit`'d in arch::init; snapshot that clean FPU state
    // as the template and seed every thread's save area with it, so a thread's
    // first FXRSTOR loads valid state rather than zeros.
    unsafe {
        crate::arch::fxsave(addr_of_mut!(FX_TEMPLATE) as *mut u8);
        for s in 0..MAX_THREADS {
            core::ptr::copy_nonoverlapping(
                addr_of!(FX_TEMPLATE) as *const u8,
                fx_area_ptr(s),
                crate::arch::FXSAVE_SIZE,
            );
        }
    }
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

fn spawn(entry: u64, arg1: u64, arg2: u64, proc: usize, cr3: u64) -> usize {
    for slot in 1..MAX_THREADS {
        // Reuse Exited slots too: an exited thread never resumes (IF=0, single
        // CPU), and init_stack rebuilds its kernel stack from the top — so the
        // slot + its static kstack are free for the next spawn.
        if matches!(state(slot), State::Free | State::Exited) {
            let (ctx_rsp, kstack_top) = init_stack(slot, entry, arg1, arg2);
            unsafe {
                *addr_of_mut!(TCBS[slot]) = Tcb {
                    state: State::Ready,
                    ctx_rsp,
                    kstack_top,
                    proc,
                    cr3,
                    wake_at: 0,
                };
                // Fresh (or reused) slot: reset its FPU state to the clean template.
                core::ptr::copy_nonoverlapping(
                    addr_of!(FX_TEMPLATE) as *const u8,
                    fx_area_ptr(slot),
                    crate::arch::FXSAVE_SIZE,
                );
            }
            return slot;
        }
    }
    panic!("thread: out of TCB slots");
}

/// Spawn a kernel thread (no owning process; runs under whatever CR3 is live).
#[allow(dead_code)] // scheduler API; kernel-thread demos were retired in arc 3
pub fn spawn_kernel(entry: extern "C" fn(u64), arg: u64) -> usize {
    spawn(entry as *const () as u64, arg, 0, NO_PROC, 0)
}

/// Register an idle thread for an AP that is ALREADY executing on `kstack_top` (its
/// dedicated bringup stack), §69 SMP Phase 5. Unlike `spawn`, it builds NO initial
/// stack frame — the AP is already running this context — it just claims a TCB slot
/// and marks it Running. Returns the tid. Called once per AP at bringup, while the
/// BSP is parked in the bringup spin-wait, so there is no concurrent TCB allocation
/// (the slot is then permanently the AP's, skipped by `spawn`'s Free/Exited scan).
pub fn register_running_idle(kstack_top: u64) -> usize {
    for slot in 1..MAX_THREADS {
        if state(slot) == State::Free {
            unsafe {
                *addr_of_mut!(TCBS[slot]) = Tcb {
                    state: State::Running,
                    ctx_rsp: 0,
                    kstack_top,
                    proc: NO_PROC,
                    cr3: 0,
                    wake_at: 0,
                };
            }
            return slot;
        }
    }
    panic!("thread: out of TCB slots (ap idle)");
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
    crate::percpu::set_current(next);
    // Point TSS.RSP0 + the syscall entry stack at the incoming thread's kernel
    // stack BEFORE the switch — safe because IF=0 throughout the kernel, so
    // nothing can trap from ring 3 between this update and the switch.
    crate::arch::set_kernel_stack(kstack_top(next));
    // Load the incoming process's address space (skip for kernel threads, cr3=0,
    // and when unchanged). Safe to reload CR3 here: the executing code, this
    // kernel stack (in .bss), and the next thread's saved context all live in
    // the shared kernel upper half present in EVERY PML4 — so nothing the switch
    // touches becomes unmapped. IF=0 means nothing interrupts mid-switch.
    let next_cr3 = cr3_of(next);
    if next_cr3 != 0 && next_cr3 != crate::arch::current_cr3() {
        announce_first_cr3(proc_of(next));
        crate::arch::load_cr3(next_cr3);
    }
    // Swap FPU/SSE state: save the outgoing thread's XMM, load the incoming
    // thread's. The kernel is soft-float and never touches XMM between here and
    // the actual switch, so the incoming state survives into ring 3.
    unsafe {
        crate::arch::fxsave(fx_area_ptr(prev));
        crate::arch::fxrstor(fx_area_ptr(next));
    }
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

/// A ring-3 fault: terminate the faulting thread AND its process (close its
/// handles, mark it Dead), then switch away. Same move as `exit_current`; the
/// kernel and every other thread continue.
pub fn kill_current_user() -> ! {
    crate::proc::kill(current_proc());
    exit_current();
}

/// Park the current thread until `wake(tid)`. The caller must already have
/// published itself wherever its waker will look (an endpoint queue, a Reply).
/// IF=0 + a single CPU make publish→Blocked→switch atomic, so no wakeup is lost.
/// INVARIANT: never called while holding a spin lock (it switches away).
pub fn block_current() {
    set_state(current(), State::Blocked);
    let next = pick_next().unwrap_or_else(|| crate::percpu::idle_tid());
    switch_to(next);
    // Woken: our waker has already deposited our result (and staging, in IPC).
}

/// Make a Blocked thread Ready. Does NOT switch — the waker keeps running.
///
/// Idempotent: waking a thread that is already Ready/Running is a no-op. This
/// matters for multi-channel waits (sys_chan_wait): a thread parked on several
/// channels can be drained+woken by one sender and then "woken" again by a second
/// sender on another channel before it runs to deregister. The first wake makes it
/// Ready; the rest are harmless.
pub fn wake(tid: usize) {
    if state(tid) == State::Blocked {
        set_state(tid, State::Ready);
    }
}

/// Arm a timer deadline (in ticks) for `tid`; the timer IRQ wakes it once
/// `TICKS >= deadline`. 0 disarms. Used for timed waits (sys_chan_wait timeout).
pub fn set_wake_at(tid: usize, deadline_tick: u64) {
    unsafe { (*addr_of_mut!(TCBS[tid])).wake_at = deadline_tick }
}

/// Has `tid`'s timer deadline passed (and is it armed)?
pub fn timed_out(tid: usize, now_tick: u64) -> bool {
    let d = unsafe { (*addr_of!(TCBS[tid])).wake_at };
    d != 0 && now_tick >= d
}

/// Timer-IRQ hook: wake every Blocked thread whose deadline has passed. Does NOT
/// clear `wake_at` — the waiter checks `timed_out()` after waking and clears it
/// itself (sys_chan_wait's exit). If we cleared it here, the waiter could not tell
/// a timer wake from a spurious one and would re-block forever. Cheap — one pass
/// over the small TCB pool, called with IF=0.
pub fn wake_expired(now_tick: u64) {
    for s in 1..MAX_THREADS {
        let d = unsafe { (*addr_of!(TCBS[s])).wake_at };
        if d != 0 && now_tick >= d && state(s) == State::Blocked {
            set_state(s, State::Ready);
        }
    }
}

/// Terminate the current thread and switch away forever.
pub fn exit_current() -> ! {
    crate::notif::clear_waiter(current()); // defensive: never wake an Exited thread
    set_state(current(), State::Exited);
    let next = pick_next().unwrap_or_else(|| crate::percpu::idle_tid());
    switch_to(next);
    unreachable!("exited thread resumed");
}

/// Called from the timer IRQ handler (IF=0). Rotate to the next Ready thread;
/// if none, keep running the current one. The preempted thread resumes through
/// the handler tail's `iretq`, back where it was interrupted.
pub fn preempt() {
    if let Some(n) = pick_next() {
        if current() != crate::percpu::idle_tid() {
            set_state(current(), State::Ready);
        }
        switch_to(n);
    }
}

/// True if any non-idle thread is still Ready, Running, or Blocked. A blocked
/// thread (e.g. a pinger waiting for its reply) is NOT quiescence. Any CPU's idle
/// thread is excluded — an AP parked on its own idle TCB isn't "work".
fn any_active() -> bool {
    (1..MAX_THREADS).any(|s| {
        matches!(state(s), State::Ready | State::Running | State::Blocked)
            && !crate::percpu::is_idle_tid(s)
    })
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

/// Spawn a user process's main thread, bound to process `proc` and address space
/// `cr3`. It enters ring 3 (IF=1) the first time it is scheduled (under its own
/// CR3) and the timer preempts it mid-userspace thereafter.
pub fn spawn_user(proc: usize, cr3: u64, entry: u64, user_rsp: u64) -> usize {
    spawn(user_thread_entry as *const () as u64, entry, user_rsp, proc, cr3)
}
