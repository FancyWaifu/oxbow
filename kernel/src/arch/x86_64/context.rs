//! The context-switch primitive and new-thread bootstrap trampoline.
//!
//! A thread's volatile state at a switch point is just the 6 callee-saved
//! registers + the return address, all living on its own kernel stack; the TCB
//! stores only the saved `rsp`. `context_switch` saves the current thread's
//! callee-saved set, swaps stacks, restores the next thread's set, and `ret`s
//! into wherever that thread last left off. A brand-new thread is bootstrapped
//! with a hand-built stack image whose `ret` lands in `thread_trampoline`.
use core::arch::naked_asm;

/// Save callee-saved regs of the current thread into `*prev_rsp_slot`, switch to
/// `next_rsp`, and restore that thread's regs. Returns on the next thread's
/// stack. `extern "C"`: rdi = prev_rsp_slot, rsi = next_rsp.
#[unsafe(naked)]
pub extern "C" fn context_switch(prev_rsp_slot: *mut u64, next_rsp: u64) {
    naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp", // save current rsp into *prev_rsp_slot
        "mov rsp, rsi",   // load next thread's rsp
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret", // return into the next thread's saved rip
    );
}

/// Entry shim for a freshly spawned thread. The hand-built stack leaves the
/// entry fn in r12 and its two arguments in r13/r14; this moves them into the
/// SysV arg registers and calls the entry. If the entry ever returns, park
/// (the scheduler's `exit_current` is what threads call instead).
#[unsafe(naked)]
pub extern "C" fn thread_trampoline() {
    naked_asm!(
        "mov rdi, r13", // arg1 -> first C argument
        "mov rsi, r14", // arg2 -> second C argument
        "call r12",     // entry(arg1, arg2)
        "2:",
        "cli",
        "hlt",
        "jmp 2b",
    );
}
