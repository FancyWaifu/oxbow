//! The `syscall` fast-path entry, the MSR setup behind it, and the first
//! descent into ring 3 (`enter_user`).
//!
//! v0 deliberately uses NO swapgs: it's a single CPU with one process and the
//! kernel never uses GS-relative addressing, so there is no per-CPU base to swap
//! (CR4.FSGSBASE is off, so user code can't install a hostile GSBASE either).
//! TWO landmines to remember when that changes: (1) the moment per-CPU data or
//! SMP arrives, every kernel entry — this stub AND every exception handler —
//! needs the swapgs dance at once; (2) there is a 1-2 instruction window at
//! entry/exit where CPL0 runs on the user `rsp`; only an NMI could exploit it,
//! and v0 has no NMI sources under QEMU. The fix later is an IST NMI handler.
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

use super::gdt;

/// The kernel stack the syscall entry stub switches to — the CURRENT thread's
/// kernel stack, updated by the scheduler on every context switch (v1). Mirrors
/// TSS.RSP0 so ring-3 traps and syscalls land on the same per-thread stack.
static mut CURRENT_KSTACK_TOP: u64 = 0;

/// Scratch slot for the user `rsp` across the stack switch. Safe as a single
/// static while there is one user thread and the kernel is non-preemptible
/// (IF=0 in all kernel code). MOVE INTO THE TCB when a second user thread lands.
static mut USER_RSP: u64 = 0;

/// Set the kernel stack the syscall entry stub switches to (the incoming
/// thread's kernel stack). Called by the scheduler on every context switch.
pub fn set_kernel_stack_top(top: u64) {
    unsafe { CURRENT_KSTACK_TOP = top };
}

/// Configure the syscall MSRs. The per-thread kernel stack and TSS.RSP0 are now
/// owned by the scheduler (set on every context switch), so init just wires the
/// MSRs. Call from `arch::init`, after `gdt::init` (it needs the selectors).
pub fn init() {
    unsafe {
        // EFER.SCE enables the `syscall`/`sysret` instructions.
        Efer::update(|f| f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
    }

    let sel = gdt::selectors();
    // Let the crate encode/validate STAR from the selectors (their RPL=3 is
    // already baked in by the GDT). Order: sysret CS/SS (user), syscall CS/SS.
    Star::write(sel.user_code, sel.user_data, sel.kernel_code, sel.kernel_data)
        .expect("STAR write (check GDT selector order)");
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    // Clear these flags on entry: IF (no kernel interrupts), DF (SysV needs
    // DF=0), TF (no single-stepping the kernel), AC (don't inherit alignment
    // checks), NT (a stale NT would #GP a future kernel iretq).
    SFMask::write(
        RFlags::INTERRUPT_FLAG
            | RFlags::DIRECTION_FLAG
            | RFlags::TRAP_FLAG
            | RFlags::ALIGNMENT_CHECK
            | RFlags::NESTED_TASK,
    );
}

/// The `syscall` entry point (LSTAR target). Naked: we control every byte.
///
/// On entry the CPU has put the user RIP in rcx and user RFLAGS in r11 (and
/// masked RFLAGS per SFMASK); nr is in rax; args in rdi, rsi, rdx, r10, r8, r9;
/// rsp is STILL the user's. We switch to the kernel entry stack, build a small
/// frame, call the Rust dispatcher (nr as the 7th, stack-passed arg so a1..a6
/// stay in their SysV registers), then restore exactly the registers ABI §4.1
/// promises to preserve — leaving rax+rdx as the two return values — and
/// `sysretq` back to ring 3.
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "mov [rip + {user_rsp}], rsp",      // stash user rsp (no GPR clobbered)
        "mov rsp, [rip + {stack_top}]",     // switch to this thread's kernel stack
        // -- 8 pushes; rsp stays 16-aligned (top is 16-aligned) --
        "push qword ptr [rip + {user_rsp}]", // user rsp
        "push r11",                          // user RFLAGS
        "push rcx",                          // user RIP
        "push rdi",                          // a1
        "push rsi",                          // a2
        "push r10",                          // a4  (a3=rdx not saved: it returns)
        "push r8",                           // a5
        "push r9",                           // a6
        "mov rcx, r10",                      // SysV arg4 (hw clobbered rcx)
        "sub rsp, 8",                        // pad -> 16-aligned at the call
        "push rax",                          // nr -> 7th C arg (on the stack)
        "call {dispatch}",
        "add rsp, 16",                       // drop nr + pad; rax/rdx now live
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rsi",
        "pop rdi",
        "pop rcx",                           // user RIP -> sysret target
        "pop r11",                           // user RFLAGS
        "pop rsp",                           // restore user rsp
        "sysretq",
        user_rsp = sym USER_RSP,
        stack_top = sym CURRENT_KSTACK_TOP,
        dispatch = sym crate::syscall::syscall_dispatch,
    );
}

/// Descend into ring 3 for the first time. This MUST be `iretq`, not `sysret`:
/// sysret can't establish SS:RSP from nothing, and iretq is the architectural
/// way to lower privilege. Never returns.
pub fn enter_user(entry: u64, user_rsp: u64) -> ! {
    let sel = gdt::selectors();
    let user_cs = sel.user_code.0 as u64; // already RPL 3 (0x23)
    let user_ss = sel.user_data.0 as u64; // already RPL 3 (0x1B)
    unsafe {
        core::arch::asm!(
            // iretq pops (low->high): RIP, CS, RFLAGS, RSP, SS. Push reverse.
            "push r12",        // SS
            "push r13",        // RSP
            "push 0x202",      // RFLAGS: reserved bit + IF=1 (ring 3 is preemptible)
            "push r14",        // CS
            "push r15",        // RIP
            // Zero every GPR so no kernel value leaks to ring 3 (rsp/rip/rflags
            // come from the frame iretq pops).
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor edi, edi",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            "iretq",
            in("r12") user_ss,
            in("r13") user_rsp,
            in("r14") user_cs,
            in("r15") entry,
            options(noreturn),
        );
    }
}
