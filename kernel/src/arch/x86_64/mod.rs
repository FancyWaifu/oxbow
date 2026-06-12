//! x86_64 architecture backend. Everything CPU- and port-specific lives behind
//! this wall, so a future aarch64 port only re-implements `arch`.
pub mod gdt;
pub mod idt;
pub mod serial;
pub mod syscall;

use core::arch::asm;

pub use serial::{write_bytes as console_write_bytes, _print};
pub use syscall::enter_user;

/// Bring up arch-level facilities: serial console, the descriptor tables
/// (GDT/TSS for the segment layout + kernel fault stack, IDT for exceptions),
/// then the syscall MSRs (and the dedicated entry stack / TSS.RSP0 repoint).
pub fn init() {
    serial::init();
    gdt::init();
    idt::init();
    syscall::init();
}

/// Trigger a breakpoint exception — used once to prove the IDT entry/return path.
pub fn breakpoint() {
    x86_64::instructions::interrupts::int3();
}

/// Switch RSP onto the static kernel stack, load the new page tables (CR3), and
/// jump into `stage2` (which must never return). The stack switch is mandatory:
/// the current stack is Limine-provided memory that may not be mapped in the new
/// tables, so the first push after `mov cr3` would otherwise fault.
pub fn switch_address_space(pml4_phys: u64, stage2: fn() -> !) -> ! {
    let stack_top = gdt::kernel_stack_top();
    unsafe {
        asm!(
            "mov rsp, {stack}",
            "mov cr3, {cr3}",
            "call {stage2}",   // call (not jmp) keeps the SysV stack alignment
            "ud2",             // unreachable: stage2 never returns
            stack = in(reg) stack_top,
            cr3 = in(reg) pml4_phys,
            stage2 = in(reg) stage2 as usize,
            options(noreturn),
        );
    }
}

/// Halt the CPU forever. `hlt` parks the core in a low-power state until an
/// interrupt — and we have none enabled yet, so this never wakes.
pub fn halt() -> ! {
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// QEMU `isa-debug-exit` status codes. The device turns a write of `code` into a
/// QEMU process exit of `(code << 1) | 1`. Used only by the (future) test harness.
#[derive(Clone, Copy)]
#[repr(u32)]
#[allow(dead_code)] // wired up when the QEMU test harness lands (post-milestone-0)
pub enum QemuExit {
    Success = 0x10,
    Failed = 0x11,
}

/// Terminate QEMU by writing to the `isa-debug-exit` device at I/O port 0xf4.
#[allow(dead_code)] // see QemuExit
pub fn exit_qemu(code: QemuExit) -> ! {
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") 0xf4u16,
            in("eax") code as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
    // If the device is absent (real hardware), fall back to halting.
    halt()
}
