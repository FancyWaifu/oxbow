//! x86_64 architecture backend. Everything CPU- and port-specific lives behind
//! this wall, so a future aarch64 port only re-implements `arch`.
pub mod context;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod lapic;
pub mod pic;
pub mod pit;
pub mod serial;
pub mod syscall;

use core::arch::asm;
use core::sync::atomic::Ordering;

pub use context::{context_switch, fxrstor, fxsave, thread_trampoline, FXSAVE_SIZE};
pub use serial::{write_bytes as console_write_bytes, _print};
pub use syscall::enter_user;

/// Program the PIT and unmask the timer IRQ line.
pub fn timer_init(hz: u32) {
    pit::init(hz);
    pic::unmask(0);
}

/// Current monotonic tick count.
#[allow(dead_code)] // scheduler heartbeat API; not currently read
pub fn ticks() -> u64 {
    idt::TICKS.load(Ordering::Relaxed)
}

/// Re-mask all IRQ lines.
#[allow(dead_code)] // API for tearing the timer back down; unused since Phase 4
pub fn timer_disable() {
    pic::mask_all();
}

/// Enable maskable interrupts (`sti`).
pub fn enable_interrupts() {
    x86_64::instructions::interrupts::enable();
}

/// Disable maskable interrupts (`cli`).
pub fn disable_interrupts() {
    x86_64::instructions::interrupts::disable();
}

/// Halt until the next interrupt (`hlt`). Only meaningful with IF=1.
pub fn wait_for_interrupt() {
    x86_64::instructions::hlt();
}

/// Bring up arch-level facilities: serial console, the descriptor tables
/// (GDT/TSS for the segment layout + kernel fault stack, IDT for exceptions),
/// then the syscall MSRs (and the dedicated entry stack / TSS.RSP0 repoint).
pub fn init() {
    serial::init();
    gdt::init();
    idt::init();
    syscall::init();
    enable_sse();
}

/// Enable SSE so userland may run SIMD code (the crypto in the DRIFT client
/// needs it — `x86_64-unknown-none` ships SSE-off so kernels can skip FPU-state
/// management, but we opt back in and save/restore the state per thread).
///
/// CR0: clear EM (no x87 emulation) + set MP; CR4: set OSFXSR (FXSAVE/FXRSTOR +
/// SSE enabled) + OSXMMEXCPT (SIMD float exceptions raise #XF, not #UD). Then
/// `fninit` brings the x87/MMX/SSE unit to a known state. The kernel itself is
/// built soft-float, so it emits no SSE — this only arms the hardware for ring 3.
pub fn enable_sse() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
    unsafe {
        let mut cr0 = Cr0::read();
        cr0.remove(Cr0Flags::EMULATE_COPROCESSOR);
        cr0.insert(Cr0Flags::MONITOR_COPROCESSOR);
        Cr0::write(cr0);
        let mut cr4 = Cr4::read();
        cr4.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE);
        Cr4::write(cr4);
        asm!("fninit", options(nostack, nomem));
    }
}

/// Trigger a breakpoint exception — used once to prove the IDT entry/return path.
pub fn breakpoint() {
    x86_64::instructions::interrupts::int3();
}

/// Point both TSS.RSP0 and the syscall entry stack at `top` — the kernel stack
/// of the thread about to run. The scheduler calls this on every context switch
/// so ring-3 traps and syscalls always land on the current thread's stack.
pub fn set_kernel_stack(top: u64) {
    gdt::set_rsp0(top);
    syscall::set_kernel_stack_top(top);
}

/// Physical address of the live PML4 (CR3).
pub fn current_cr3() -> u64 {
    use x86_64::registers::control::Cr3;
    Cr3::read().0.start_address().as_u64()
}

/// Load a new address space root into CR3 (preserving the current CR3 flags).
pub fn load_cr3(pml4_phys: u64) {
    use x86_64::registers::control::Cr3;
    use x86_64::structures::paging::PhysFrame;
    use x86_64::PhysAddr;
    let (_, flags) = Cr3::read();
    let frame = PhysFrame::containing_address(PhysAddr::new(pml4_phys));
    unsafe { Cr3::write(frame, flags) };
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

/// Unmask a PIC IRQ line (re-arm it — used by `irq::ack`).
pub fn pic_unmask(line: u8) {
    pic::unmask(line);
}

/// Read a byte from an I/O port (backs `sys_io_in`).
pub fn io_in(port: u16) -> u8 {
    unsafe { x86_64::instructions::port::Port::<u8>::new(port).read() }
}

/// Write a byte to an I/O port (backs `sys_io_out`).
pub fn io_out(port: u16, value: u8) {
    unsafe { x86_64::instructions::port::Port::<u8>::new(port).write(value) }
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
