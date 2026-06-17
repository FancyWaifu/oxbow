//! GDT + per-CPU TSS.
//!
//! The selector order is NOT free: the `syscall`/`sysret` MSR (STAR) requires
//! kernel code/data and user data/code in a specific arrangement (see
//! `syscall.rs`). The order below — kernel code, kernel data, user data, user
//! code — is exactly what SYSRET's "base + 8 / base + 16" arithmetic expects, so
//! the `x86_64` crate's append order is load-bearing. Don't reorder the first four.
//!
//! §69 SMP Phase 5: there is one **TSS per CPU**, each with its own RSP0 (the
//! ring-0 stack the CPU loads on a ring-3→0 trap) and its own #DF IST stack. A TSS
//! can only be `ltr`'d on one CPU (loading sets the descriptor's busy bit), so each
//! CPU gets a distinct TSS descriptor (appended after the four segment descriptors)
//! and loads its own. The BSP builds the whole table in `init`; each AP just points
//! its GDTR at it and `ltr`s its own TSS (`load_ap`).
use core::ptr::{addr_of, addr_of_mut};
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use crate::percpu::MAX_CPUS;

/// Kernel stack used for exceptions taken from ring 3 (loaded via TSS.RSP0).
const KERNEL_STACK_SIZE: usize = 32 * 1024;
/// Dedicated stack for #DF, so a stack fault can't turn into a triple fault.
const DF_STACK_SIZE: usize = 16 * 1024;

/// Index into the IST for the double-fault handler.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// GDT capacity: the null descriptor, four segment descriptors, plus a two-entry
/// system (TSS) descriptor for every possible CPU.
const GDT_CAP: usize = 1 + 4 + 2 * MAX_CPUS;

#[repr(align(16))]
struct Stack<const N: usize>([u8; N]);

/// Per-CPU ring-0 exception/syscall stacks (RSP0) and #DF IST stacks. Indexed by
/// CPU index; CPU 0 is the BSP. ~384 KiB of .bss for MAX_CPUS — cheap and static.
static mut KERNEL_STACKS: [Stack<KERNEL_STACK_SIZE>; MAX_CPUS] =
    [const { Stack([0; KERNEL_STACK_SIZE]) }; MAX_CPUS];
static mut DF_STACKS: [Stack<DF_STACK_SIZE>; MAX_CPUS] =
    [const { Stack([0; DF_STACK_SIZE]) }; MAX_CPUS];

static mut TSS: [TaskStateSegment; MAX_CPUS] = [const { TaskStateSegment::new() }; MAX_CPUS];
static mut GDT: GlobalDescriptorTable<GDT_CAP> = GlobalDescriptorTable::empty();

/// Selectors we reload after `lgdt`. Kept for the syscall MSR setup. `tss` is the
/// BSP's TSS selector (CPU 0); per-CPU TSS selectors live in `TSS_SELECTORS`.
#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub tss: SegmentSelector,
}

static mut SELECTORS: Option<Selectors> = None;
/// The TSS selector for each CPU, filled in `init` (one per appended TSS).
static mut TSS_SELECTORS: [Option<SegmentSelector>; MAX_CPUS] = [None; MAX_CPUS];

fn kernel_stack_top_of(cpu: usize) -> u64 {
    unsafe { addr_of!(KERNEL_STACKS[cpu]) as u64 + KERNEL_STACK_SIZE as u64 }
}
fn df_stack_top_of(cpu: usize) -> u64 {
    unsafe { addr_of!(DF_STACKS[cpu]) as u64 + DF_STACK_SIZE as u64 }
}

/// Top of the BSP's kernel exception stack — used by `switch_address_space` as the
/// stage-2 stack (runs on the BSP, before per-CPU state exists).
pub fn kernel_stack_top() -> u64 {
    kernel_stack_top_of(0)
}

/// Build every CPU's TSS + the GDT, load them on the BSP, reload its segment
/// registers, and `ltr` the BSP's TSS. Runs once, on the BSP.
pub fn init() {
    unsafe {
        // Seed every CPU's TSS with its own RSP0 + #DF IST stack up front, so an AP
        // only has to `ltr` its (already-valid) TSS in `load_ap`.
        for cpu in 0..MAX_CPUS {
            let tss = &mut (*addr_of_mut!(TSS))[cpu];
            tss.privilege_stack_table[0] = VirtAddr::new(kernel_stack_top_of(cpu));
            tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
                VirtAddr::new(df_stack_top_of(cpu));
        }

        // Append order defines the selectors (see module docs): the four segment
        // descriptors first, then one TSS descriptor per CPU.
        let gdt = &mut *addr_of_mut!(GDT);
        let kernel_code = gdt.append(Descriptor::kernel_code_segment()); // 0x08
        let kernel_data = gdt.append(Descriptor::kernel_data_segment()); // 0x10
        let user_data = gdt.append(Descriptor::user_data_segment()); //     0x18
        let user_code = gdt.append(Descriptor::user_code_segment()); //     0x20
        for cpu in 0..MAX_CPUS {
            let tss_ref: &'static TaskStateSegment = &(*addr_of!(TSS))[cpu];
            (*addr_of_mut!(TSS_SELECTORS))[cpu] = Some(gdt.append(Descriptor::tss_segment(tss_ref)));
        }

        let sel = Selectors {
            kernel_code,
            kernel_data,
            user_data,
            user_code,
            tss: (*addr_of!(TSS_SELECTORS))[0].unwrap(),
        };
        *addr_of_mut!(SELECTORS) = Some(sel);

        let gdt_ref: &'static GlobalDescriptorTable<GDT_CAP> = &*addr_of!(GDT);
        gdt_ref.load();

        // Reload CS via far return, data segments directly, then the BSP's TSS.
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

/// Load the (already-built) GDT, reload segment registers, and `ltr` CPU `cpu`'s
/// OWN TSS on an Application Processor (§69 SMP Phase 5). Each CPU has a distinct
/// TSS descriptor, so the busy bit never collides. The BSP built the table in
/// `init`; the AP just points its GDTR at it and loads its segments + TSS.
pub fn load_ap(cpu: usize) {
    unsafe {
        let sel = selectors();
        let gdt_ref: &'static GlobalDescriptorTable<GDT_CAP> = &*addr_of!(GDT);
        gdt_ref.load();
        use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
        use x86_64::instructions::tables::load_tss;
        CS::set_reg(sel.kernel_code);
        DS::set_reg(sel.kernel_data);
        ES::set_reg(sel.kernel_data);
        SS::set_reg(sel.kernel_data);
        load_tss((*addr_of!(TSS_SELECTORS))[cpu].expect("gdt::init must run before load_ap"));
    }
}

/// Repoint THIS CPU's TSS.RSP0 (the stack the CPU loads on a ring-3 → ring-0
/// exception/syscall). The scheduler calls this on every context switch with the
/// incoming thread's kernel stack. Per-CPU: writes the running CPU's own TSS, so
/// two cores scheduling at once never clobber each other's RSP0. Safe after `ltr`:
/// the CPU reads RSP0 from the in-memory TSS on each privilege change.
pub fn set_rsp0(top: u64) {
    let cpu = crate::percpu::cpu_index();
    unsafe {
        let tss = &mut (*addr_of_mut!(TSS))[cpu];
        tss.privilege_stack_table[0] = VirtAddr::new(top);
    }
}
