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
    // Point this AP's GDTR/IDTR at the shared tables and reload its segments (no
    // TSS — see gdt::load_ap), then claim its per-CPU state (GS base) and enable
    // its LAPIC. enable() reuses the LAPIC MMIO mapping the BSP already made.
    crate::arch::load_descriptor_tables_ap();
    crate::percpu::init(index);
    let lapic_id = crate::arch::lapic::enable();
    AP_LAPIC_ID[index].store(lapic_id, Ordering::Release);
    AP_ONLINE[index].store(true, Ordering::Release);

    // Park forever. IF stays 0: this core must not take interrupts (its timer is
    // unstarted and device IRQs route to the BSP), because the scheduler, IPC, and
    // frame allocator are not yet SMP-safe. It simply halts — alive, but idle.
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

/// Bring up exactly ONE Application Processor into the idle loop above. Called by
/// the BSP from `kmain_stage2`, after it is fully on the kernel PML4 with its own
/// LAPIC enabled (the AP reuses the LAPIC MMIO mapping). Bounded-spins until the
/// AP reports in, then logs it. The BSP continues normally regardless.
pub fn bring_up_one(mp: &MpResponse) {
    // Publish the kernel PML4 the AP must switch onto.
    KERNEL_PML4.store(crate::arch::current_cr3(), Ordering::Release);

    let bsp = mp.bsp_lapic_id();
    let Some(cpu) = mp.cpus().iter().copied().find(|c| c.lapic_id != bsp) else {
        println!("[smp] no AP to bring up (uniprocessor)");
        return;
    };
    let index = 1usize; // BSP holds per-CPU slot 0; the first AP takes slot 1.

    // Stash the AP's index where ap_entry will read it, then launch. write() does a
    // SeqCst store that publishes everything above it before the core jumps.
    cpu.extra.store(index as u64, Ordering::Release);
    cpu.goto_address.write(ap_entry);

    // Wait (bounded) for the AP to finish its setup. A live second core completes
    // this in microseconds; the cap just stops us hanging if bringup faults.
    let mut spins: u64 = 0;
    while !AP_ONLINE[index].load(Ordering::Acquire) {
        core::hint::spin_loop();
        spins += 1;
        if spins > 500_000_000 {
            println!("[smp] AP {} did not come online (timeout)", index);
            return;
        }
    }
    println!(
        "[smp] AP {} online (lapic_id={}) — parked in idle on its own core",
        index,
        AP_LAPIC_ID[index].load(Ordering::Acquire),
    );
}
