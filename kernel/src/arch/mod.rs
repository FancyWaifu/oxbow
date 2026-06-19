//! Architecture abstraction wall. The backend is chosen by `target_arch`; the
//! rest of the kernel uses only the names re-exported here and never refers to
//! an ISA directly. Adding aarch64 later means adding a sibling module and a
//! `cfg` arm — nothing above this line changes.
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::lapic; // §69 SMP: Local APIC (enable/eoi/timer)
#[cfg(target_arch = "x86_64")]
pub use x86_64::ioapic; // §69 SMP: I/O APIC (device IRQ routing)

#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)] // exit_qemu/QemuExit are for the future test harness
pub use x86_64::{
    breakpoint, console_write_bytes, context_switch, current_cr3, disable_interrupts,
    enable_interrupts, enter_user, exit_qemu, fxrstor, fxsave, halt, init, io_in, io_out, load_cr3,
    init_ap_cpu, load_descriptor_tables_ap, panic_print, pic_unmask, set_fs_base, set_kernel_stack,
    switch_address_space, thread_trampoline, ticks, timer_disable, timer_init, walltime,
    wait_for_interrupt, FXSAVE_SIZE, QemuExit, _print,
};
