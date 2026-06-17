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
}
impl PerCpu {
    const fn new() -> Self {
        PerCpu { cpu_index: 0, current: 0, idle_tid: 0 }
    }
}
// The asm accessors below hardcode these offsets; keep them honest.
const _: () = assert!(core::mem::offset_of!(PerCpu, cpu_index) == 0);
const _: () = assert!(core::mem::offset_of!(PerCpu, current) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, idle_tid) == 16);

static mut PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// Point this CPU's GS base at `PERCPU[index]` and stamp its index. Called once per
/// CPU — the BSP in stage 2, each AP at bringup (Phase 3).
pub fn init(index: usize) {
    unsafe {
        let p = &mut (*addr_of_mut!(PERCPU))[index];
        p.cpu_index = index;
        p.current = 0;
        Msr::new(IA32_GS_BASE).write(p as *mut PerCpu as u64);
    }
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

/// True if `tid` is some CPU's idle thread. Scans the PerCpu pool directly (not
/// via `gs:`), so it works from any CPU. Used by the scheduler's quiescence check
/// to ignore idle threads. Note: unstarted CPUs default `idle_tid` to 0, which
/// correctly matches the BSP's idle (TCB 0); an AP's idle id only registers as
/// idle once that AP has set it.
pub fn is_idle_tid(tid: usize) -> bool {
    unsafe { (*addr_of_mut!(PERCPU)).iter().any(|p| p.idle_tid == tid) }
}
