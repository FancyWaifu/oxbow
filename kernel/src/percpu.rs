//! Per-CPU state (§69 SMP Phase 4) — the prerequisite for running threads on more
//! than one core. Each CPU needs its OWN "current thread" (and, later, its own idle
//! thread and run queue); a single global `CURRENT` only works on one core.
//!
//! The state lives in a `PerCpu` struct found through the **GS base** — the standard
//! x86_64 per-CPU mechanism. oxbow uses NO `swapgs` (user code never touches GS and
//! CR4.FSGSBASE is off), so the kernel simply points `IA32_GS_BASE` at this CPU's
//! `PerCpu` and accesses its fields directly via the `gs:` prefix — fast (no rdmsr on
//! the hot path) and valid in interrupt/syscall context alike. Each CPU touches only
//! its own slot, so no locking is needed.
use core::ptr::addr_of_mut;
use x86_64::registers::model_specific::Msr;

const IA32_GS_BASE: u32 = 0xC000_0101;
pub const MAX_CPUS: usize = 8;

#[repr(C)]
pub struct PerCpu {
    /// This CPU's index into the pool (0 = BSP). At `gs:[0]`.
    pub cpu_index: usize,
    /// The thread currently running on THIS CPU (was the global `thread::CURRENT`).
    /// At `gs:[8]`.
    pub current: usize,
    /// This CPU's idle thread (the TCB it runs when its run queue is empty). The
    /// BSP's is TCB 0; each AP registers its own. Was the hardcoded `thread::IDLE`.
    /// At `gs:[16]`.
    pub idle_tid: usize,
    /// Kernel stack the `syscall` entry stub switches to — the running thread's
    /// kernel stack, updated by the scheduler on every context switch (§72; was the
    /// global `syscall::CURRENT_KSTACK_TOP`). At `gs:[24]` — read directly by the
    /// naked syscall stub, so this offset is load-bearing.
    pub kstack_top: u64,
    /// Scratch for the user `rsp` across the syscall stack switch (was the global
    /// `syscall::USER_RSP`). Per-CPU so two cores entering `syscall` at once can't
    /// clobber each other. At `gs:[32]` — read/written by the naked stub.
    pub user_rsp: u64,
}
impl PerCpu {
    const fn new() -> Self {
        PerCpu { cpu_index: 0, current: 0, idle_tid: 0, kstack_top: 0, user_rsp: 0 }
    }
}
// The asm accessors AND the naked syscall stub hardcode these offsets; keep honest.
const _: () = assert!(core::mem::offset_of!(PerCpu, cpu_index) == 0);
const _: () = assert!(core::mem::offset_of!(PerCpu, current) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, idle_tid) == 16);
const _: () = assert!(core::mem::offset_of!(PerCpu, kstack_top) == 24);
const _: () = assert!(core::mem::offset_of!(PerCpu, user_rsp) == 32);

static mut PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// §78: true once a CPU's GS base is set, so `cpu_index()`/`current()` (gs-relative)
/// are safe. Gates `DiagMutex` instrumentation off during early BSP boot (before
/// `init`) where the GS base is still 0 and a `gs:` read would fault.
static PERCPU_READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Is per-CPU GS state set up (so `gs:`-relative reads are safe)? See `PERCPU_READY`.
#[inline]
pub fn ready() -> bool {
    PERCPU_READY.load(core::sync::atomic::Ordering::Acquire)
}

/// Point this CPU's GS base at `PERCPU[index]` and stamp its index. Called once per
/// CPU — the BSP in stage 2, each AP at bringup (Phase 3).
pub fn init(index: usize) {
    unsafe {
        let p = &mut (*addr_of_mut!(PERCPU))[index];
        p.cpu_index = index;
        p.current = 0;
        Msr::new(IA32_GS_BASE).write(p as *mut PerCpu as u64);
    }
    // The BSP (init(0), in stage 2) makes gs: reads safe for all subsequent code;
    // each AP has its own GS set before it touches any DiagMutex.
    PERCPU_READY.store(true, core::sync::atomic::Ordering::Release);
}

/// This CPU's index (0 = BSP).
#[inline]
pub fn cpu_index() -> usize {
    let v: usize;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) v, options(nostack, preserves_flags, readonly)) };
    v
}

/// The thread currently running on THIS CPU.
#[inline]
pub fn current() -> usize {
    let v: usize;
    unsafe { core::arch::asm!("mov {}, gs:[8]", out(reg) v, options(nostack, preserves_flags, readonly)) };
    v
}

/// Set the thread running on THIS CPU.
#[inline]
pub fn set_current(tid: usize) {
    unsafe { core::arch::asm!("mov gs:[8], {}", in(reg) tid, options(nostack, preserves_flags)) };
}

/// THIS CPU's idle thread tid.
#[inline]
pub fn idle_tid() -> usize {
    let v: usize;
    unsafe { core::arch::asm!("mov {}, gs:[16]", out(reg) v, options(nostack, preserves_flags, readonly)) };
    v
}

/// Set THIS CPU's idle thread tid (the BSP in `thread::init`, each AP at bringup).
#[inline]
pub fn set_idle_tid(tid: usize) {
    unsafe { core::arch::asm!("mov gs:[16], {}", in(reg) tid, options(nostack, preserves_flags)) };
}

/// Set the kernel stack the `syscall` stub switches to (the running thread's
/// kernel stack). Called by the scheduler on every context switch, on the CPU that
/// will run the thread — so it lands in THAT CPU's PerCpu (gs:[24]).
#[inline]
pub fn set_kstack_top(top: u64) {
    unsafe { core::arch::asm!("mov gs:[24], {}", in(reg) top, options(nostack, preserves_flags)) };
}

/// True if `tid` is some CPU's idle thread. Scans the PerCpu pool directly (not
/// via `gs:`), so it works from any CPU. Used by the scheduler's quiescence check
/// to ignore idle threads. Note: unstarted CPUs default `idle_tid` to 0, which
/// correctly matches the BSP's idle (TCB 0); an AP's idle id only registers as
/// idle once that AP has set it.
pub fn is_idle_tid(tid: usize) -> bool {
    unsafe { (*addr_of_mut!(PERCPU)).iter().any(|p| p.idle_tid == tid) }
}
