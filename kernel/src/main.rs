//! oxbow microkernel — milestone-0 ("first light").
//!
//! Boots via Limine, brings up the serial console, prints a banner, and halts.
//! This is deliberately ABI-neutral: it proves the toolchain -> boot -> QEMU
//! loop works before any of the capability machinery in docs/abi-v0.md exists.
#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

mod arch;
mod elf;
mod ipc;
mod mm;
mod object;
mod proc;
mod syscall;
mod thread;
mod usermem;

use core::panic::PanicInfo;
use limine::request::{
    ExecutableAddressRequest, HhdmRequest, MemoryMapRequest, ModuleRequest, RequestsEndMarker,
    RequestsStartMarker,
};
use limine::BaseRevision;

/// Print to the serial console (no newline).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::arch::_print(format_args!($($arg)*)));
}

/// Print to the serial console, with a trailing newline.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// Declares which base revision of the Limine boot protocol we speak. The
/// bootloader reads (and patches) this in the `.requests` section.
#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();

/// Markers bounding the request section so Limine scans only our requests.
#[used]
#[link_section = ".requests_start_marker"]
static _REQ_START: RequestsStartMarker = RequestsStartMarker::new();
#[used]
#[link_section = ".requests_end_marker"]
static _REQ_END: RequestsEndMarker = RequestsEndMarker::new();

/// Higher-half direct map — lets the kernel reach any physical frame.
#[used]
#[link_section = ".requests"]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

/// The firmware/bootloader memory map — what RAM we actually have.
#[used]
#[link_section = ".requests"]
static MEMMAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

/// Where Limine loaded the kernel image (phys + virt base), so we can re-map it.
#[used]
#[link_section = ".requests"]
static EXEC_ADDR_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

/// The user-mode server binaries Limine loaded for us (the v0 initrd).
#[used]
#[link_section = ".requests"]
static MODULE_REQUEST: ModuleRequest = ModuleRequest::new();

/// Kernel entry point. Limine jumps here — see `ENTRY(kmain)` in linker.ld —
/// with a 64-bit, higher-half environment already established.
#[no_mangle]
extern "C" fn kmain() -> ! {
    arch::init();

    // Refuse to continue under a bootloader that doesn't speak our protocol.
    assert!(
        BASE_REVISION.is_supported(),
        "Limine base revision not supported by this bootloader"
    );

    println!();
    println!("  oxbow :: secure-minimal capability microkernel");
    println!("  v0 -- building toward PONG");
    println!();

    // -- Phase 1: physical memory --------------------------------------------
    let hhdm = HHDM_REQUEST
        .get_response()
        .expect("limine: no HHDM response");
    mm::set_hhdm_offset(hhdm.offset());

    let memmap = MEMMAP_REQUEST
        .get_response()
        .expect("limine: no memory map response");
    let (usable, regions) = mm::pmm::init(memmap);

    println!("[mm] hhdm @ {:#018x}", hhdm.offset());
    println!(
        "[mm] usable: {} MiB across {} regions",
        usable / (1024 * 1024),
        regions
    );

    // Prove the HHDM math before paging starts depending on it: grab a frame,
    // write a sentinel through the direct map, read it back.
    let frame = mm::pmm::alloc_frame().expect("pmm: out of memory");
    let sentinel = unsafe {
        let p = mm::phys_to_virt(frame) as *mut u32;
        p.write_volatile(0xDEAD_BEEF);
        p.read_volatile()
    };
    println!(
        "[pmm] test frame @ phys {:#x} ; wrote/read {:#010x} via hhdm: {}",
        frame,
        sentinel,
        if sentinel == 0xDEAD_BEEF { "ok" } else { "FAIL" }
    );

    // -- Phase 2: CPU tables (GDT/TSS/IDT) -----------------------------------
    // The descriptor tables came up in arch::init(). Prove the IDT works by
    // taking a breakpoint and returning from it — recoverable, unlike the
    // faulting exceptions which dump-and-panic.
    println!("[trap] testing IDT via int3...");
    arch::breakpoint();
    println!("[ ok ] cpu tables: gdt+tss loaded, idt armed");

    // -- Phase 3: kernel-owned page tables -----------------------------------
    // Build our own tables (kernel image W^X-clean + HHDM), then switch CR3.
    // Control continues in kmain_stage2 on the static kernel stack.
    let kaddr = EXEC_ADDR_REQUEST
        .get_response()
        .expect("limine: no kernel address response");
    let pml4 = mm::vm::init(memmap, kaddr.physical_base(), kaddr.virtual_base());
    println!("[vm] kernel tables built: text RX, rodata R+NX, data RW+NX, hhdm RW+NX");
    arch::switch_address_space(pml4, kmain_stage2);
}

/// Runs after the CR3 switch, on the kernel's own page tables and static stack.
/// That this prints at all proves `.text` (this code) and `.data` (the serial
/// port statics) are correctly mapped in the new address space.
fn kmain_stage2() -> ! {
    println!("[vm] cr3 switched -- still alive");

    // The boot thread becomes the idle thread (TCB 0).
    thread::init();

    // -- v1 arc 2 Phase 1: address-space construction primitive --------------
    // Prove a second PML4 (sharing the kernel upper half) can be hopped into and
    // back. Runs BEFORE the timer is armed, so IF=0 guarantees no trap mid-hop.
    mm::vm::as_hop_selftest();

    arch::timer_init(100); // PIT @ 100 Hz, IRQ0 unmasked (stays on)

    ipc::init();

    // One process per Limine module, each in its OWN address space (a fresh
    // PML4 sharing the kernel upper half). Both binaries link at 0x200000 but
    // never collide — that's the point.
    let resp = MODULE_REQUEST
        .get_response()
        .expect("limine: no module response");
    let mods = resp.modules();
    println!("[mod] {} module(s) loaded", mods.len());

    for (i, file) in mods.iter().enumerate() {
        let bytes = unsafe { core::slice::from_raw_parts(file.addr(), file.size() as usize) };
        let name = match i {
            0 => "P1",
            1 => "P2",
            2 => "P3",
            _ => "P4",
        };
        println!("[mod] module {} ({}): {} bytes", i, name, bytes.len());
        let img = elf::Image::validate(bytes);
        let as_i = mm::vm::new_user_pml4();
        let (pid, entry, user_rsp) = proc::create(&img, as_i, name);
        let tcb = thread::spawn_user(pid, as_i, entry, user_rsp);
        println!("[user] {} scheduled as tcb {} (ring 3, IF=1)", name, tcb);
    }

    let w = thread::spawn_witness();
    println!("[thr] witness = tcb {}", w);

    // The boot thread becomes the idle thread and runs the scheduler forever.
    thread::run_idle();
}

/// Bare-metal panic handler: report to serial, then halt. There is no unwinding.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\n[PANIC] {}", info);
    arch::halt();
}
