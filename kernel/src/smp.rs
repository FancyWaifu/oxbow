//! SMP application-processor bringup (§69 Phase 3).
//!
//! Limine has already started every AP and parked it, waiting for us to write its
//! `goto_address`. When we do, that core jumps to `ap_entry` — but still on the
//! BOOTLOADER's page tables and Limine's temporary stack. So the first thing each
//! AP does (in asm, atomically) is switch onto the kernel's PML4 and its own
//! dedicated kernel stack, exactly as the BSP did in `switch_address_space`; only
//! then is it safe to run Rust that touches the stack.
//!
//! Phase 3 is deliberately minimal: the AP loads the shared GDT/IDT, claims its
//! per-CPU state (GS base), enables its own LAPIC, reports in, and then **halts
//! with interrupts disabled forever**. It runs NO scheduler, IPC, or allocator
//! code — none of that is SMP-safe until the Phase 5 locking work — and it takes
//! no interrupts (its LAPIC timer is left unstarted). This proves the kernel can
//! launch and execute code on a second core without yet sharing any mutable state.
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use limine::mp::Cpu;
use limine::response::MpResponse;

use crate::percpu::MAX_CPUS;
use crate::println;

/// Physical address of the kernel PML4 (CR3), published by the BSP so each AP can
/// switch onto it before running any kernel code.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// Set true by an AP once it has reached `ap_main` and finished its own setup.
static AP_ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Each AP records its LAPIC id here for the BSP to log (the AP doesn't print —
/// the serial console isn't lock-protected yet, so only the BSP writes to it).
static AP_LAPIC_ID: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// A dedicated kernel stack per CPU slot, in `.bss` (so it's mapped in the kernel
/// PML4 via the kernel image / HHDM). The AP switches RSP onto its slot before it
/// runs any Rust. 16 KiB is ample for a core that only idles.
const AP_STACK_SIZE: usize = 16 * 1024;
#[repr(align(16))]
struct ApStack([u8; AP_STACK_SIZE]);
static mut AP_STACKS: [ApStack; MAX_CPUS] = [const { ApStack([0; AP_STACK_SIZE]) }; MAX_CPUS];

/// Top (initial RSP) of CPU `index`'s dedicated kernel stack. 16-byte aligned.
fn ap_stack_top(index: usize) -> u64 {
    unsafe { core::ptr::addr_of!(AP_STACKS[index]) as u64 + AP_STACK_SIZE as u64 }
}

/// First instructions an AP runs (Limine jumps here). STILL on the bootloader's
/// page tables and stack — so we read the few values we need, then switch RSP +
/// CR3 and tail-call `ap_main` in one asm block. Nothing after the switch may
/// touch the old stack, hence `options(noreturn)` and the `ud2` backstop.
unsafe extern "C" fn ap_entry(cpu: &Cpu) -> ! {
    let index = cpu.extra.load(Ordering::Acquire) as usize;
    let stack_top = ap_stack_top(index);
    let cr3 = KERNEL_PML4.load(Ordering::Acquire);
    core::arch::asm!(
        "mov rsp, {stack}",   // onto our own kernel stack (mapped in both tables via HHDM)
        "mov cr3, {cr3}",     // onto the kernel PML4 (LAPIC/percpu/HHDM mappings live there)
        "mov rdi, {idx}",     // ap_main(index) — SysV first arg
        "call {main}",
        "ud2",                // ap_main never returns
        stack = in(reg) stack_top,
        cr3 = in(reg) cr3,
        idx = in(reg) index,
        main = sym ap_main,
        options(noreturn),
    );
}

/// Runs on the AP once it's on the kernel PML4 and its own stack, IF=0.
extern "C" fn ap_main(index: usize) -> ! {
    // Point this AP's GDTR/IDTR at the shared tables, reload its segments, and `ltr`
    // its OWN TSS (§69 Phase 5 — its own RSP0 + #DF stack). Then claim its per-CPU
    // state (GS base) and register its idle thread: the AP IS its idle thread,
    // already running on this dedicated stack, so `current` becomes that TCB.
    crate::arch::load_descriptor_tables_ap(index);
    // §72: replay the per-CPU CPU-feature init (syscall MSRs + SSE) so a user thread
    // scheduled here doesn't #UD on its first syscall or SSE instruction.
    crate::arch::init_ap_cpu();
    crate::percpu::init(index);
    let idle = crate::thread::register_running_idle(ap_stack_top(index));
    crate::percpu::set_idle_tid(idle);
    crate::percpu::set_current(idle);
    // Enable this AP's LAPIC (reuses the LAPIC MMIO mapping the BSP already made).
    let lapic_id = crate::arch::lapic::enable();
    AP_LAPIC_ID[index].store(lapic_id, Ordering::Release);
    AP_ONLINE[index].store(true, Ordering::Release);

    // §72: this AP now SCHEDULES. Start its LAPIC timer (TIMER_COUNT is already
    // calibrated by the BSP, so this reuses it — no PIT access from the AP) and
    // enter the scheduler idle loop. Its timer tick drives `preempt`, which pulls
    // Ready threads off the shared run queue under SCHED_LOCK (§71) and runs real
    // user work on this core. The lost-wakeup protocol (§70) + the context-switch
    // handoff (§71) + per-CPU syscall stacks (§72) make this safe across cores.
    crate::arch::lapic::start_timer(crate::arch::lapic::TIMER_VECTOR, 100);
    crate::thread::run_idle();
}

/// Bring up EVERY available Application Processor into the scheduler. Called by the
/// BSP from `kmain_stage2`, after it is fully on the kernel PML4 with its own LAPIC
/// enabled (each AP reuses the LAPIC MMIO mapping). APs are started one at a time —
/// launch, bounded-spin until it reports online, then the next — which keeps bringup
/// serialised (no two cores initialising at once) and bounds the per-AP risk. Each
/// gets a sequential per-CPU index 1..N; the BSP is 0. Capped at `MAX_CPUS` (the size
/// of the per-CPU stack/TSS/state pools); any extra cores stay parked by Limine.
pub fn bring_up_all(mp: &MpResponse) {
    // Publish the kernel PML4 every AP must switch onto.
    KERNEL_PML4.store(crate::arch::current_cr3(), Ordering::Release);

    let bsp = mp.bsp_lapic_id();
    let mut index = 1usize; // BSP holds per-CPU slot 0; APs take 1, 2, 3, ...
    let mut online = 0usize;
    let mut parked = 0usize;

    for cpu in mp.cpus().iter().copied() {
        if cpu.lapic_id == bsp {
            continue; // skip the BSP — it's already running this code
        }
        if index >= MAX_CPUS {
            parked += 1; // more cores than our per-CPU pools hold; leave parked
            continue;
        }

        // Stash this AP's index where ap_entry reads it, then launch. write() does a
        // SeqCst store that publishes everything above it before the core jumps.
        cpu.extra.store(index as u64, Ordering::Release);
        cpu.goto_address.write(ap_entry);

        // Wait (bounded) for THIS AP to finish setup before launching the next, so
        // bringup is serialised. A live core completes in microseconds; the cap just
        // stops us hanging if one faults during bringup.
        let mut spins: u64 = 0;
        while !AP_ONLINE[index].load(Ordering::Acquire) {
            core::hint::spin_loop();
            spins += 1;
            if spins > 500_000_000 {
                break;
            }
        }
        if AP_ONLINE[index].load(Ordering::Acquire) {
            println!(
                "[smp] AP {} online (lapic_id={}) — scheduling",
                index,
                AP_LAPIC_ID[index].load(Ordering::Acquire),
            );
            online += 1;
        } else {
            println!("[smp] AP {} did not come online (timeout)", index);
        }
        index += 1;
    }

    if online == 0 && parked == 0 {
        println!("[smp] no AP to bring up (uniprocessor)");
    } else {
        println!(
            "[smp] {} core(s) scheduling (BSP + {} AP(s)){}",
            online + 1,
            online,
            if parked > 0 { " — extra cores parked (> MAX_CPUS)" } else { "" },
        );
    }
}
