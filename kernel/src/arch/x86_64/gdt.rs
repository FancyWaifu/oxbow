//! GDT + TSS.
//!
//! The selector order is NOT free: the `syscall`/`sysret` MSR (STAR) requires
//! kernel code/data and user data/code in a specific arrangement (see
//! `syscall.rs`). The order below — kernel code, kernel data, user data, user
//! code — is exactly what SYSRET's "base + 8 / base + 16" arithmetic expects, so
//! the `x86_64` crate's append order is load-bearing. Don't reorder.
use core::ptr::{addr_of, addr_of_mut};
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// Kernel stack used for exceptions taken from ring 3 (loaded via TSS.RSP0).
const KERNEL_STACK_SIZE: usize = 32 * 1024;
/// Dedicated stack for #DF, so a stack fault can't turn into a triple fault.
const DF_STACK_SIZE: usize = 16 * 1024;

/// Index into the IST for the double-fault handler.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

#[repr(align(16))]
struct Stack<const N: usize>([u8; N]);

static mut KERNEL_STACK: Stack<KERNEL_STACK_SIZE> = Stack([0; KERNEL_STACK_SIZE]);
static mut DF_STACK: Stack<DF_STACK_SIZE> = Stack([0; DF_STACK_SIZE]);

static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

/// Selectors we reload after `lgdt`. Kept for the syscall MSR setup.
#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub tss: SegmentSelector,
}

static mut SELECTORS: Option<Selectors> = None;

/// Top of the kernel exception stack (what TSS.RSP0 points at).
pub fn kernel_stack_top() -> u64 {
    addr_of!(KERNEL_STACK) as u64 + KERNEL_STACK_SIZE as u64
}

/// Build the TSS + GDT, load them, reload segment registers, and load the TSS.
pub fn init() {
    unsafe {
        // RSP0: where the CPU switches to on an exception from CPL3.
        let tss = &mut *addr_of_mut!(TSS);
        tss.privilege_stack_table[0] = VirtAddr::new(kernel_stack_top());
        // IST1: a separate, always-valid stack for #DF.
        let df_top = addr_of!(DF_STACK) as u64 + DF_STACK_SIZE as u64;
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = VirtAddr::new(df_top);

        let tss_ref: &'static TaskStateSegment = &*addr_of!(TSS);

        // Append order defines the selectors (see module docs).
        let gdt = &mut *addr_of_mut!(GDT);
        let kernel_code = gdt.append(Descriptor::kernel_code_segment()); // 0x08
        let kernel_data = gdt.append(Descriptor::kernel_data_segment()); // 0x10
        let user_data = gdt.append(Descriptor::user_data_segment()); //     0x18
        let user_code = gdt.append(Descriptor::user_code_segment()); //     0x20
        let tss_sel = gdt.append(Descriptor::tss_segment(tss_ref)); //      0x28

        let sel = Selectors {
            kernel_code,
            kernel_data,
            user_data,
            user_code,
            tss: tss_sel,
        };
        *addr_of_mut!(SELECTORS) = Some(sel);

        let gdt_ref: &'static GlobalDescriptorTable = &*addr_of!(GDT);
        gdt_ref.load();

        // Reload CS via far return, data segments directly, then the TSS.
        use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
        use x86_64::instructions::tables::load_tss;
        CS::set_reg(sel.kernel_code);
        DS::set_reg(sel.kernel_data);
        ES::set_reg(sel.kernel_data);
        SS::set_reg(sel.kernel_data);
        load_tss(sel.tss);
    }
}

/// The selectors chosen at `init`, for the syscall MSR setup.
pub fn selectors() -> Selectors {
    unsafe { (*addr_of!(SELECTORS)).expect("gdt::init must run before selectors()") }
}

/// Repoint TSS.RSP0 (the stack the CPU loads on a ring-3 → ring-0 exception).
/// Phase 4 moves this off the boot thread's KERNEL_STACK onto the dedicated
/// syscall entry stack. Safe after `ltr`: the CPU reads RSP0 from the in-memory
/// TSS on each privilege change, so no reload is needed.
pub fn set_rsp0(top: u64) {
    unsafe {
        let tss = &mut *addr_of_mut!(TSS);
        tss.privilege_stack_table[0] = VirtAddr::new(top);
    }
}
