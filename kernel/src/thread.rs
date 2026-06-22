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
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

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
// §104: bumped 32 -> 256 so a thread-heavy std program (e.g. a libtest run, or an
// mpsc stress test spawning ~100 senders) doesn't exhaust the pool. ~4 MiB of BSS.
pub const MAX_THREADS: usize = 256;
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
#[repr(u8)]
pub enum State {
    Free = 0,
    Ready = 1,
    Running = 2,
    Blocked = 3,
    Exited = 4,
}
impl State {
    #[inline]
    fn from_u8(v: u8) -> State {
        match v {
            0 => State::Free,
            1 => State::Ready,
            2 => State::Running,
            3 => State::Blocked,
            _ => State::Exited,
        }
    }
}

/// Sentinel for "no owning process" (kernel threads: idle, witness).
pub const NO_PROC: usize = usize::MAX;

#[derive(Clone, Copy)]
struct Tcb {
    ctx_rsp: u64,    // saved stack pointer (the whole context lives on the stack)
    kstack_top: u64,
    proc: usize,     // owning process id, or NO_PROC for kernel threads
    cr3: u64,        // address-space root to load on switch (0 = keep live CR3)
    /// Timer deadline in ticks (0 = none). When `TICKS >= wake_at`, the timer IRQ
    /// wakes this Blocked thread. Used for timed waits (sys_chan_wait timeout).
    wake_at: u64,
    /// §96 futex: the user address this thread is `SYS_FUTEX_WAIT`-blocked on
    /// (0 = not futex-waiting). `SYS_FUTEX_WAKE` scans for matches in the same proc.
    futex_addr: u64,
    /// §101 native ELF TLS: this thread's %fs base (thread pointer), loaded into
    /// IA32_FS_BASE on switch-in. 0 = no TLS (kernel threads / TLS-less images).
    fs_base: u64,
}

static mut TCBS: [Tcb; MAX_THREADS] = [Tcb {
    ctx_rsp: 0,
    kstack_top: 0,
    proc: NO_PROC,
    cr3: 0,
    wake_at: 0,
    futex_addr: 0,
    fs_base: 0,
}; MAX_THREADS];

// §70 SMP lost-wakeup fix: the thread STATE lives in its own atomic array, NOT in
// the (plain, struct-copied) Tcb. Making state atomic is what lets the sleep/wake
// protocol below be correct across cores — a waker on another CPU stores Ready
// while the sleeper loads its own state, with no torn reads or data race. This is
// the same separation Linux (task->__state, READ_ONCE/WRITE_ONCE) and the BSDs
// (p_stat under SCHED_LOCK) rely on. `State::Free` == 0, so the zero-init is Free.
static STATE: [AtomicU8; MAX_THREADS] = [const { AtomicU8::new(State::Free as u8) }; MAX_THREADS];

// §103 Command::kill: a per-thread "self-terminate" flag. Set on every thread of a
// killed process; checked at safe resume points (preempt + after block_current),
// where the thread calls `exit_current` — which terminates it OFF its own kernel
// stack (the only SMP-safe way; externally forcing a running thread Exited races
// with kernel-stack reuse). Cleared when a slot is (re)spawned.
static SHOULD_DIE: [core::sync::atomic::AtomicBool; MAX_THREADS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; MAX_THREADS];

// §69 Phase 4: the running thread is PER-CPU — it lives in this CPU's PerCpu
// (reached via the GS base), not a single global. `current()`/`set_running()`
// funnel through `crate::percpu`. On one core this is identical to the old global.

// --- TCB field accessors (all access to the static-mut pool funnels here) ---
fn state(s: usize) -> State {
    State::from_u8(STATE[s].load(Ordering::Acquire))
}
fn set_state(s: usize, st: State) {
    STATE[s].store(st as u8, Ordering::Release);
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
fn fs_base_of(s: usize) -> u64 {
    unsafe { (*addr_of!(TCBS[s])).fs_base }
}
/// The calling thread's FS base (TLS pointer) — fork hands it to the child, whose
/// TLS block was copied with the address space at the same virtual address.
pub fn current_fs_base() -> u64 {
    fs_base_of(current())
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

/// Claim a TCB slot and start a thread. Returns the tid (>= 1), or 0 if the pool is
/// exhausted (§104) — a userland spawn must NEVER panic the kernel, so the caller
/// turns 0 into an error the program handles (std `thread::spawn` -> `Err`).
fn spawn(entry: u64, arg1: u64, arg2: u64, proc: usize, cr3: u64, fs_base: u64) -> usize {
    // §72: under SCHED_LOCK — two cores (a user thread on each) can `sys_spawn`
    // concurrently, and a scheduler on another core may be scanning for Ready
    // threads. The lock makes "claim a Free/Exited slot, fill it, publish Ready"
    // atomic, so no two cores grab the same slot and no scheduler can pick a
    // half-initialised TCB.
    sched_lock();
    for slot in 1..MAX_THREADS {
        // Reuse Exited slots too: an exited thread never resumes, and init_stack
        // rebuilds its kernel stack from the top — so the slot + its static kstack
        // are free for the next spawn.
        if matches!(state(slot), State::Free | State::Exited) {
            let (ctx_rsp, kstack_top) = init_stack(slot, entry, arg1, arg2);
            SHOULD_DIE[slot].store(false, Ordering::Relaxed); // fresh slot starts clean
            unsafe {
                *addr_of_mut!(TCBS[slot]) = Tcb {
                    ctx_rsp,
                    kstack_top,
                    proc,
                    cr3,
                    wake_at: 0,
                    futex_addr: 0,
                    fs_base,
                };
                // Fresh (or reused) slot: reset its FPU state to the clean template.
                core::ptr::copy_nonoverlapping(
                    addr_of!(FX_TEMPLATE) as *const u8,
                    fx_area_ptr(slot),
                    crate::arch::FXSAVE_SIZE,
                );
            }
            // Publish as runnable LAST (Release): a scheduler that sees Ready
            // (Acquire) is then guaranteed to see the fully-written TCB above.
            set_state(slot, State::Ready);
            sched_unlock();
            return slot;
        }
    }
    sched_unlock();
    0 // pool exhausted — caller returns an error to userland (never a kernel panic)
}

/// Spawn a kernel thread (no owning process; runs under whatever CR3 is live).
#[allow(dead_code)] // scheduler API; kernel-thread demos were retired in arc 3
pub fn spawn_kernel(entry: extern "C" fn(u64), arg: u64) -> usize {
    spawn(entry as *const () as u64, arg, 0, NO_PROC, 0, 0)
}

/// Register an idle thread for an AP that is ALREADY executing on `kstack_top` (its
/// dedicated bringup stack), §69 SMP Phase 5. Unlike `spawn`, it builds NO initial
/// stack frame — the AP is already running this context — it just claims a TCB slot
/// and marks it Running. Returns the tid. Called once per AP at bringup, while the
/// BSP is parked in the bringup spin-wait, so there is no concurrent TCB allocation
/// (the slot is then permanently the AP's, skipped by `spawn`'s Free/Exited scan).
pub fn register_running_idle(kstack_top: u64) -> usize {
    sched_lock(); // §72: consistent with spawn() — exclusive TCB-slot allocation
    for slot in 1..MAX_THREADS {
        if state(slot) == State::Free {
            unsafe {
                *addr_of_mut!(TCBS[slot]) = Tcb {
                    ctx_rsp: 0,
                    kstack_top,
                    proc: NO_PROC,
                    cr3: 0,
                    wake_at: 0,
                    futex_addr: 0,
                    fs_base: 0,
                };
            }
            set_state(slot, State::Running); // the AP is already running on this stack
            sched_unlock();
            return slot;
        }
    }
    sched_unlock();
    panic!("thread: out of TCB slots (ap idle)");
}

// --- The scheduler lock (§71 SMP mechanism B) -------------------------------
// A single global spinlock serializing ALL run-queue decisions and the context
// switch itself — OpenBSD's `SCHED_LOCK` model (simpler than Linux's per-CPU rq
// locks + `p->on_cpu` spin, and a better fit for this minimal kernel). The lock is
// held ACROSS `context_switch` and released by whatever thread *resumes* (its own
// `switch_to` tail, or `thread_trampoline` for a freshly spawned thread) — exactly
// Linux's `finish_task_switch` / OpenBSD's "the new thread drops SCHED_LOCK"
// handoff. Because it spans the switch, no other CPU can `pick_next` a thread until
// that thread's context is fully saved — so a woken thread is never resumed on two
// cores at once. It is a raw lock (not an RAII guard): acquire and release happen
// in different stack frames / different threads, so a guard can't model it.
//
// Discipline: only ever taken alone (never while holding a per-structure lock; the
// wait sites drop their interlock before `block_current`), and always released
// before the CPU returns to IF=1 — so the timer IRQ can never fire while a CPU
// holds it, and there is no lock-order cycle.
// CANONICAL KERNEL LOCK ORDER (§73 audit — acquire high→low, NEVER the reverse):
//   ENDPOINTS > PROCESSES > REPLIES > REGIONS > SCHED_LOCK > BINDINGS
//             > { CONNS, PIPES, POOL, RNG, IMAGES, MEMORY, FRAMES, BUMP }  (leaves)
//             > SERIAL  (bottom — pure I/O, acquires nothing).
// The graph is acyclic, so cross-CPU spinning never deadlocks. The deeper reason:
// every kernel critical section runs IF=0 (SFMask on syscall; IRQ gates), so a core
// never takes an IRQ while holding a lock — only OTHER cores spin, and they always
// progress. See docs/smp-arc.md "Lock-ordering audit" for the full edge list.
static SCHED_LOCK: AtomicBool = AtomicBool::new(false);

/// One-shot: set the first time an AP context-switches to a user thread (§72 proof).
static AP_RAN_USER: AtomicBool = AtomicBool::new(false);

/// §74 — the `on_cpu` flag (Linux `task->on_cpu`): which CPU each TCB is currently
/// running on, or -1 if none. Maintained in `switch_to` UNDER SCHED_LOCK (cleared
/// for the outgoing thread, set for the incoming one). `pick_next` consults it so a
/// thread that was woken (`Ready`) while STILL executing on its core — the window the
/// §70 lost-wakeup fix opens between `prepare_block` and the actual context save — is
/// NOT pickable by another core until it has truly switched off (which clears this
/// under SCHED_LOCK, after `context_switch` saves its context). Without this, a core
/// could resume a still-running thread from a stale saved context: the same thread on
/// two cores at once.
static RUNNING_ON: [core::sync::atomic::AtomicI8; MAX_THREADS] =
    [const { core::sync::atomic::AtomicI8::new(-1) }; MAX_THREADS];

/// §75 DEBUG: which CPU currently holds SCHED_LOCK (-1 = free), for the deadlock report.
static SCHED_HOLDER: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);

fn sched_lock() {
    let mut spins: u64 = 0;
    while SCHED_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // §78: count EVERY failed CAS (outer loop), so a LIVELOCK — where the lock is
        // briefly free each retry so the inner load-spin never accumulates — is also
        // caught, not just a held-forever deadlock.
        spins += 1;
        if spins > 400_000_000 {
            crate::deadlock_report(
                "SCHED_LOCK",
                crate::percpu::cpu_index() as i32,
                SCHED_HOLDER.load(Ordering::Relaxed),
            );
        }
        while SCHED_LOCK.load(Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }
    SCHED_HOLDER.store(crate::percpu::cpu_index() as i32, Ordering::Relaxed);
}

fn sched_unlock() {
    SCHED_HOLDER.store(-1, Ordering::Relaxed);
    SCHED_LOCK.store(false, Ordering::Release);
}

/// Drop `SCHED_LOCK` from the new-thread trampoline (C ABI, called from asm). A
/// freshly spawned thread is switched-to with the lock held but does NOT resume
/// inside `switch_to`, so it must release the lock here instead — the trampoline's
/// equivalent of `finish_task_switch`.
pub extern "C" fn sched_unlock_c() {
    sched_unlock();
}

/// Round-robin scan for the next Ready thread after CURRENT (never returns
/// CURRENT, never returns the idle thread unless it's explicitly Ready).
/// MUST be called with `SCHED_LOCK` held.
fn pick_next() -> Option<usize> {
    let cur = current();
    for off in 1..MAX_THREADS {
        let s = (cur + off) % MAX_THREADS;
        // Ready AND fully off its previous core (§74 on_cpu): a thread woken while
        // still running there isn't safe to resume here until its context is saved.
        if state(s) == State::Ready && RUNNING_ON[s].load(Ordering::Acquire) == -1 {
            return Some(s);
        }
    }
    None
}

/// Save the current context and resume `next`. Caller sets the outgoing thread's
/// state first (Ready/Exited) and MUST hold `SCHED_LOCK`; this function releases it
/// (after the switch, in the resumed thread — or immediately if there is no switch).
fn switch_to(next: usize) {
    let prev = current();
    if prev == next {
        sched_unlock(); // nothing to switch to; release the lock the caller took
        return;
    }
    // §74 on_cpu maintenance (under SCHED_LOCK): prev leaves this CPU, next arrives.
    // Clearing prev here — before `context_switch` saves its context — is safe because
    // SCHED_LOCK is held across the whole switch, so no other core can `pick_next` prev
    // until the lock is released (after the save). The `was != -1` check is a cheap
    // invariant guard: it must never fire (that would be the same thread on two cores).
    {
        let cpu = crate::percpu::cpu_index() as i8;
        RUNNING_ON[prev].store(-1, Ordering::Release);
        let was = RUNNING_ON[next].swap(cpu, Ordering::AcqRel);
        if was != -1 {
            println!("[BUG] double-run: tcb {} was on cpu {}, now cpu {}", next, was, cpu);
        }
    }
    set_state(next, State::Running);
    crate::percpu::set_current(next);
    // §72 one-shot proof that an AP actually runs USER work (not just idles).
    {
        let cpu = crate::percpu::cpu_index();
        if cpu != 0 && proc_of(next) != NO_PROC && !AP_RAN_USER.swap(true, Ordering::Relaxed) {
            println!("[smp] AP cpu {} is now running user threads (proc {})", cpu, proc_of(next));
        }
    }
    // Point TSS.RSP0 + the syscall entry stack at the incoming thread's kernel
    // stack BEFORE the switch — safe because IF=0 throughout the kernel, so
    // nothing can trap from ring 3 between this update and the switch.
    crate::arch::set_kernel_stack(kstack_top(next));
    // §101 native ELF TLS: load the incoming thread's %fs base (its TLS thread
    // pointer). Per-thread MSR state, not saved on the kernel stack — set it on
    // every switch-in from the Tcb. 0 for kernel/TLS-less threads (harmless).
    crate::arch::set_fs_base(fs_base_of(next));
    // Load the incoming process's address space (skip for kernel threads, cr3=0,
    // and when unchanged). Safe to reload CR3 here: the executing code, this
    // kernel stack (in .bss), and the next thread's saved context all live in
    // the shared kernel upper half present in EVERY PML4 — so nothing the switch
    // touches becomes unmapped. IF=0 means nothing interrupts mid-switch.
    let next_cr3 = cr3_of(next);
    if next_cr3 != 0 && next_cr3 != crate::arch::current_cr3() {
        // §77 DEBUG: catch a CORRUPT cr3 BEFORE loading it — a bad PML4 root triple-
        // faults the instant we MOV it into CR3 (the fault handler is then unmapped,
        // so it hangs silently). Check (a) the root is a sane phys addr, and (b) the
        // PML4 actually maps the kernel higher half (entry 256 PRESENT) — if not, the
        // PML4 is stale/freed/uninitialised (a use-after-free or new_user_pml4 race).
        let bad_addr = next_cr3 & 0xfff != 0 || next_cr3 >= 0x2000_0000;
        let entry256 = if bad_addr {
            0
        } else {
            unsafe { core::ptr::read_volatile((crate::mm::phys_to_virt(next_cr3) as *const u64).add(256)) }
        };
        if bad_addr || entry256 & 1 == 0 {
            crate::cr3_bug(next, next_cr3, proc_of(next), current(), entry256);
        }
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
    // We (prev) have just been RESUMED by some future `switch_to(prev)` on some CPU,
    // which handed us `SCHED_LOCK`. Release it — the finish_task_switch handoff. (A
    // freshly spawned thread never reaches here; it releases in thread_trampoline.)
    sched_unlock();
}

/// Cooperatively yield to the next Ready thread (no-op if none). Unused in this
/// arc (preemption replaced it) but kept as a scheduler primitive.
#[allow(dead_code)]
pub fn yield_now() {
    sched_lock();
    match pick_next() {
        Some(n) => {
            set_state(current(), State::Ready);
            switch_to(n); // releases SCHED_LOCK
        }
        None => sched_unlock(),
    }
}

/// A ring-3 fault: terminate the faulting thread AND its process (close its
/// handles, mark it Dead), then switch away. Same move as `exit_current`; the
/// kernel and every other thread continue.
pub fn kill_current_user() -> ! {
    crate::proc::kill(current_proc(), 139); // 128 + SIGSEGV: faulted in ring 3
    exit_current();
}

/// Phase 1 of going to sleep — the lost-wakeup fix (§70). Mark the current thread
/// `Blocked` and emit a full barrier, BEFORE the caller does its final readiness
/// re-check or anything that exposes it to a waker. This is exactly Linux's
/// `set_current_state()` (a store + `smp_mb`) and OpenBSD's `sleep_setup()`: the
/// store-load barrier orders our Blocked-store ahead of the condition-load, so it
/// pairs with the waker (which stores the condition, then loads our state in
/// `wake`). A waker on ANOTHER CPU therefore can't slip between our check and our
/// sleep and be lost — it will see `Blocked` and flip us back to `Ready`.
///
/// USAGE (the canonical sleep loop, same shape as `wait_event` / `msleep`):
/// ```ignore
/// loop {
///     publish_self_where_the_waker_looks();   // e.g. endpoint queue, notif waiter
///     thread::prepare_block();                 // set Blocked + barrier
///     if condition_ready() { thread::cancel_block(); consume(); break; }
///     thread::block_current();                 // sleep ONLY if still Blocked
/// }
/// ```
pub fn prepare_block() {
    set_state(current(), State::Blocked);
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// Undo `prepare_block` without sleeping — the condition came true between phase 1
/// and now (OpenBSD `sleep_finish` with `do_sleep == 0`). Back to Running.
pub fn cancel_block() {
    set_state(current(), State::Running);
}

/// Phase 2 — actually sleep (OpenBSD `sleep_finish` / Linux `schedule`). Switch
/// away ONLY if we are still `Blocked`: if a waker on another CPU already flipped
/// us to `Ready` after `prepare_block`, we must NOT sleep (that wakeup would be
/// lost) — we just return and the caller re-checks. The caller MUST have called
/// `prepare_block()` first and dropped any interlock. Never holds a spin lock here.
pub fn block_current() {
    sched_lock();
    if state(current()) != State::Blocked {
        // A wake raced in after prepare_block — already Ready; don't sleep.
        set_state(current(), State::Running);
        sched_unlock();
        return;
    }
    let next = pick_next().unwrap_or_else(|| crate::percpu::idle_tid());
    switch_to(next); // releases SCHED_LOCK (in the resumed thread)
    // Woken: our waker has already deposited our result (and staging, in IPC).
    // §103: if our process was killed while we slept, self-terminate now — the
    // kill woke us precisely so we'd reach this safe exit point.
    if SHOULD_DIE[current()].load(Ordering::Relaxed) {
        exit_current();
    }
}

/// The CAS at the heart of `wake` (OpenBSD `setrunnable` / Linux `try_to_wake_up`):
/// `Blocked → Ready`, no switch. MUST be called with `SCHED_LOCK` held. Idempotent —
/// if the thread isn't Blocked (still running, or already woken) the CAS fails and
/// no-ops; a wake that lands while the sleeper is still Running is caught by the
/// post-`prepare_block` condition re-check, not lost.
fn wake_locked(tid: usize) {
    let _ = STATE[tid].compare_exchange(
        State::Blocked as u8,
        State::Ready as u8,
        Ordering::AcqRel,
        Ordering::Relaxed,
    );
}

/// Make a Blocked thread Ready. §76: the state transition now runs **under
/// SCHED_LOCK**, like Linux `ttwu` (state changed under `pi_lock`/`rq_lock`) and
/// FreeBSD `wakeup` (under the thread/sleepqueue lock) — NOT a lock-free CAS racing
/// the scheduler. Serializing the wakeup with `pick_next`/`switch_to` removes a
/// class of cross-core state races the lock-free wake exposed. Lock order: callers
/// hold either no lock or a structure lock ABOVE SCHED_LOCK (e.g. ENDPOINTS), never
/// a leaf below it — so `… > SCHED_LOCK` stays acyclic (§73).
pub fn wake(tid: usize) {
    sched_lock();
    wake_locked(tid);
    sched_unlock();
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
    // §76: take SCHED_LOCK ONCE for the whole scan (not per-thread). Called from the
    // BSP timer IRQ, which is not under SCHED_LOCK, so this is a fresh acquire.
    sched_lock();
    for s in 1..MAX_THREADS {
        let d = unsafe { (*addr_of!(TCBS[s])).wake_at };
        if d != 0 && now_tick >= d {
            wake_locked(s);
        }
    }
    sched_unlock();
}

/// Terminate the current thread and switch away forever.
pub fn exit_current() -> ! {
    crate::notif::clear_waiter(current()); // defensive: never wake an Exited thread (drops POOL)
    sched_lock();
    // §74: mark Exited UNDER SCHED_LOCK, not before it. Otherwise another core's
    // `spawn()` (also under SCHED_LOCK) could see this slot Exited and `init_stack`
    // a fresh frame onto our kernel stack WHILE we are still running `exit_current`
    // on it — clobbering our return addresses (a #GP with a corrupt rip). Holding
    // SCHED_LOCK across the Exited-store AND the switch means the slot only becomes
    // reusable after `switch_to` has carried us off this stack (it releases the lock
    // from the next thread), so the reuse can't race our last instructions here.
    set_state(current(), State::Exited);
    let next = pick_next().unwrap_or_else(|| crate::percpu::idle_tid());
    switch_to(next); // never returns to us; the next thread releases SCHED_LOCK
    unreachable!("exited thread resumed");
}

/// §103 Command::kill: flag every live thread of `proc` to self-terminate, and wake
/// the blocked ones so they reach the exit check in `block_current`. Running/Ready
/// threads hit the check in `preempt` on their next tick. The threads exit OFF their
/// own stacks (safe); the process itself is reaped separately by `proc::kill`.
pub fn mark_proc_dying(proc: usize) {
    sched_lock();
    for s in 1..MAX_THREADS {
        if proc_of(s) == proc {
            match state(s) {
                State::Free | State::Exited => {}
                st => {
                    SHOULD_DIE[s].store(true, Ordering::Relaxed);
                    if st == State::Blocked {
                        wake_locked(s);
                    }
                }
            }
        }
    }
    sched_unlock();
}

/// True if `proc` still owns any non-Exited thread. `proc::create` uses this to
/// avoid reusing a Dead process slot (freeing its address space) while a killed
/// thread is still winding down on it — which would be a use-after-free (§103).
pub fn proc_has_live_threads(proc: usize) -> bool {
    (1..MAX_THREADS).any(|s| {
        proc_of(s) == proc && !matches!(state(s), State::Free | State::Exited)
    })
}

/// Called from the timer IRQ handler (IF=0). Rotate to the next Ready thread;
/// if none, keep running the current one. The preempted thread resumes through
/// the handler tail's `iretq`, back where it was interrupted.
pub fn preempt() {
    sched_lock();
    // §103: a thread whose process was killed self-terminates here (off its own
    // kernel stack — safe). Catches threads that were Running/Ready at kill time.
    if current() != crate::percpu::idle_tid() && SHOULD_DIE[current()].load(Ordering::Relaxed) {
        sched_unlock(); // exit_current re-takes the lock
        exit_current();
    }
    if let Some(n) = pick_next() {
        if current() != crate::percpu::idle_tid() {
            set_state(current(), State::Ready);
        }
        switch_to(n); // releases SCHED_LOCK
    } else {
        sched_unlock();
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

/// The idle thread body — never returns. Parks for ticks; the timer handler
/// reschedules to any Ready thread. We resume here only when nothing else is
/// runnable on THIS CPU. Run by the BSP (TCB 0) and, since §72, by every AP on
/// its own idle TCB — so each core pulls Ready work via `preempt`. The quiescence
/// announcement is BSP-only (cpu 0), to avoid two cores racing on the message.
pub fn run_idle() -> ! {
    let is_bsp = crate::percpu::cpu_index() == 0;
    let mut quiescent = false;
    loop {
        if any_active() {
            quiescent = false;
        } else if !quiescent && is_bsp {
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
pub fn spawn_user(proc: usize, cr3: u64, entry: u64, user_rsp: u64, fs_base: u64) -> usize {
    spawn(user_thread_entry as *const () as u64, entry, user_rsp, proc, cr3, fs_base)
}

/// §96: spawn a new thread in the CALLER's address space (shares its `cr3` + owning
/// process) with a caller-provided user stack. Backs `SYS_THREAD_SPAWN` — the
/// kernel half of `std::thread::spawn`. Returns the new tid. §101: builds a fresh
/// per-thread TLS block (the caller's AS is the process's, so `.tdata` is live).
pub fn spawn_thread_in_current(entry: u64, user_rsp: u64) -> usize {
    let cur = current();
    let (proc, cr3) = unsafe {
        let t = &*addr_of!(TCBS[cur]);
        (t.proc, t.cr3)
    };
    let fs_base = crate::proc::build_thread_tls(proc);
    spawn_user(proc, cr3, entry, user_rsp, fs_base)
}

/// §96 futex wait — block the caller until a `futex_wake` on `addr` arrives, but
/// only if `*addr == expected` (the compare-and-block that closes the lost-wakeup
/// race). `addr` is a user vaddr live in the current address space. Uses the same
/// prepare/re-check/block protocol as the rest of the kernel.
pub fn futex_wait(addr: u64, expected: u32, timeout_ms: u64) -> bool {
    let cur = current();
    unsafe {
        (*addr_of_mut!(TCBS[cur])).futex_addr = addr;
    }
    // §97 timeout: arm a timer deadline; `wake_expired` (timer IRQ) wakes us when it
    // passes. 100 Hz PIT → 1 tick = 10 ms; round up, at least 1 tick. 0 = no timeout.
    let deadline = if timeout_ms != 0 {
        // Overflow-safe: `(timeout_ms + 9)` wraps for huge values (std passes u64::MAX
        // for an "infinite" wait_timeout), which would set the deadline to ~now and make
        // every wait time out immediately — std then busy-loops re-waiting and starves the
        // waker. `div_ceil` + `saturating_add` keep a huge timeout effectively infinite, so
        // the thread blocks until a real `futex_wake`.
        let d = crate::arch::ticks().saturating_add(timeout_ms.div_ceil(10));
        set_wake_at(cur, d);
        d
    } else {
        0
    };
    prepare_block();
    // Re-check AFTER publishing Blocked: pairs with a waker that stores `*addr`
    // then calls `futex_wake` (which loads our state under SCHED_LOCK).
    let now = unsafe { (addr as *const u32).read_volatile() };
    if now != expected {
        cancel_block();
        unsafe {
            (*addr_of_mut!(TCBS[cur])).futex_addr = 0;
        }
        set_wake_at(cur, 0);
        return false;
    }
    block_current();
    // Woken by a `futex_wake`, or (if armed) the timer once the deadline passed.
    let timed_out = deadline != 0 && crate::arch::ticks() >= deadline;
    unsafe {
        (*addr_of_mut!(TCBS[cur])).futex_addr = 0;
    }
    set_wake_at(cur, 0);
    timed_out
}

/// §96: tid of the calling thread — backs `SYS_THREAD_ID` (std's keyed TLS keys
/// per-thread storage on this).
pub fn current_tid() -> usize {
    current()
}

/// Set the calling thread's FS base (x86_64 TLS pointer) and apply it to the CPU
/// immediately. Backs `SYS_SET_FSBASE` / musl's `arch_prctl(ARCH_SET_FS)`. The new
/// base is stored in the TCB so it survives context switches (restored at line ~431).
pub fn set_fsbase_current(base: u64) {
    let cur = current();
    unsafe { (*addr_of_mut!(TCBS[cur])).fs_base = base };
    crate::arch::set_fs_base(base);
}

/// §96 thread exit with an optional join signal. If `done_addr != 0`, store
/// `*done_addr = 1` and futex-wake it FIRST — done from kernel mode, when the
/// thread is already off its user stack, so a joiner woken by this can free that
/// stack without racing the exiting thread's last instructions. Then `exit_current`.
pub fn thread_exit(done_addr: u64) -> ! {
    if done_addr != 0 {
        unsafe {
            (done_addr as *mut u32).write_volatile(1);
        }
        futex_wake(done_addr, usize::MAX);
    }
    exit_current()
}

/// §96 futex wake — wake up to `count` threads of the caller's process that are
/// blocked on `addr`. Returns how many were woken. A linear TCB scan (the pool is
/// small), the same shape as `wake_expired`.
pub fn futex_wake(addr: u64, count: usize) -> usize {
    let proc = current_proc();
    let mut woken = 0;
    sched_lock();
    for s in 1..MAX_THREADS {
        if woken >= count {
            break;
        }
        let (t_proc, t_addr) = unsafe {
            let t = &*addr_of!(TCBS[s]);
            (t.proc, t.futex_addr)
        };
        if t_proc == proc && t_addr == addr && state(s) == State::Blocked {
            unsafe {
                (*addr_of_mut!(TCBS[s])).futex_addr = 0;
            }
            wake_locked(s);
            woken += 1;
        }
    }
    sched_unlock();
    woken
}
