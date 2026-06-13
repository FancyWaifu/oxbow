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
mod image;
mod ipc;
mod irq;
mod mm;
mod notif;
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

    // The timer-driven tick notification (granted to module 0 as BOOT_TICK).
    let tick_idx = notif::create().expect("tick notif");
    notif::arm_tick(tick_idx);

    // One process per Limine module, each in its OWN address space (a fresh
    // PML4 sharing the kernel upper half). Both binaries link at 0x200000 but
    // never collide — that's the point.
    let resp = MODULE_REQUEST
        .get_response()
        .expect("limine: no module response");
    let mods = resp.modules();
    println!("[mod] {} module(s) loaded", mods.len());

    // The tar initrd (mapped into the fs server's AS below). Captured as (phys
    // base, size); module load addresses are page-aligned (Limine).
    let mut initrd: Option<(u64, u64)> = None;

    for file in mods.iter() {
        let bytes: &'static [u8] =
            unsafe { core::slice::from_raw_parts(file.addr(), file.size() as usize) };
        let cmd = file.string().to_bytes(); // the module_cmdline = the program name
        let name = core::str::from_utf8(cmd).unwrap_or("?");

        // The filesystem initrd: not a program — remember where it is so we can
        // map it into the fs server's address space when we spawn it.
        if cmd == b"initrd" {
            let phys = file.addr() as u64 - mm::hhdm_offset();
            initrd = Some((phys, file.size()));
            println!("[mod] initrd: {} bytes @ phys {:#x}", file.size(), phys);
            continue;
        }

        // Demo / on-demand programs are NOT boot-spawned: they are registered as
        // spawnable Image capabilities and launched later from the shell. This is
        // what gives a clean boot straight to the prompt (no demo spam).
        if matches!(
            cmd,
            b"pong" | b"beta" | b"hello" | b"badge" | b"cat" | b"ls" | b"mkdir" | b"touch" | b"rm" | b"mv" | b"cp"
        ) {
            image::register(cmd, bytes);
            println!("[mod] image '{}' registered ({} bytes)", name, bytes.len());
            continue;
        }

        println!("[mod] module '{}': {} bytes", name, bytes.len());
        let img = elf::Image::validate(bytes);
        let as_i = mm::vm::new_user_pml4();
        let (pid, entry, user_rsp) = proc::create(&img, as_i, name).expect("boot: create");
        // The shell is the system's spawner; it needs a large budget to fund the
        // processes it launches (it pays from its own untyped). Drivers get 256K.
        let budget = if cmd == b"shell" { 8 * 1024 * 1024 } else { mm::mem::BOOT_BUDGET };
        proc::grant_standard(pid, budget);
        // The kbd driver gets the i8042 I/O ports + IRQ line as capabilities. The
        // kernel is the root of hardware authority; it delegates here (L1 holds
        // — authority lives in a handle, not a global).
        if cmd == b"kbd" {
            let io_rights = oxbow_abi::R_IN
                | oxbow_abi::R_OUT
                | oxbow_abi::R_GRANT
                | oxbow_abi::R_ATTENUATE;
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_IRQ,
                    object::HandleEntry {
                        obj: object::ObjectRef::Irq(1), // keyboard line
                        rights: oxbow_abi::R_BIND
                            | oxbow_abi::R_ACK
                            | oxbow_abi::R_GRANT
                            | oxbow_abi::R_ATTENUATE,
                    badge: 0,
                    },
                );
                p.install(
                    oxbow_abi::BOOT_KBD_DATA,
                    object::HandleEntry {
                        obj: object::ObjectRef::IoPort { base: 0x60, len: 1 },
                        rights: io_rights,
                    badge: 0,
                    },
                );
                p.install(
                    oxbow_abi::BOOT_KBD_STATUS,
                    object::HandleEntry {
                        obj: object::ObjectRef::IoPort { base: 0x64, len: 1 },
                        rights: io_rights,
                    badge: 0,
                    },
                );
                // The kbd driver sends characters to the TTY endpoint.
                p.install(
                    oxbow_abi::BOOT_TTY,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP1),
                        rights: oxbow_abi::R_SEND,
                    badge: 0,
                    },
                );
            });
        }
        // The tty is the sole receiver on the TTY endpoint.
        if cmd == b"tty" {
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_TTY,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP1),
                        rights: oxbow_abi::R_RECV,
                    badge: 0,
                    },
                )
            });
        }
        // The shell sends read-requests + output to the TTY endpoint, and nothing
        // else: its default Console grant is REVOKED here. All shell output flows
        // through the tty (TAG_TTY_WRITE), so it holds zero direct hardware
        // authority. R_GRANT|R_ATTENUATE on the TTY handle so it can mint an
        // attenuated send-only "stdout" endpoint for the programs it spawns.
        if cmd == b"shell" {
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_TTY,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP1),
                        rights: oxbow_abi::R_SEND | oxbow_abi::R_GRANT | oxbow_abi::R_ATTENUATE,
                    badge: 0,
                    },
                );
                // Drop the console: the shell must not write hardware directly.
                let _ = p.close(oxbow_abi::BOOT_CONSOLE);
                // The timer tick, so the shell can delegate an attenuated clock
                // to a spawned pong (which waits on it). R_GRANT to hand it on.
                p.install(
                    oxbow_abi::BOOT_TICK,
                    object::HandleEntry {
                        obj: object::ObjectRef::Notification(tick_idx),
                        rights: oxbow_abi::R_WAIT | oxbow_abi::R_GRANT | oxbow_abi::R_ATTENUATE,
                    badge: 0,
                    },
                );
                // Spawnable program images (capabilities — the shell can only
                // launch what it was granted). R_SPAWN to spawn, R_GRANT|R_ATTEN
                // so a future init/launcher could hand them on further.
                let spawn_rights = oxbow_abi::R_SPAWN | oxbow_abi::R_GRANT | oxbow_abi::R_ATTENUATE;
                for (handle, iname) in [
                    (oxbow_abi::BOOT_IMG_HELLO, b"hello".as_slice()),
                    (oxbow_abi::BOOT_IMG_PONG, b"pong".as_slice()),
                    (oxbow_abi::BOOT_IMG_BETA, b"beta".as_slice()),
                    (oxbow_abi::BOOT_IMG_BADGE, b"badge".as_slice()),
                    (oxbow_abi::BOOT_IMG_CAT, b"cat".as_slice()),
                    (oxbow_abi::BOOT_IMG_LS, b"ls".as_slice()),
                    (oxbow_abi::BOOT_IMG_MKDIR, b"mkdir".as_slice()),
                    (oxbow_abi::BOOT_IMG_TOUCH, b"touch".as_slice()),
                    (oxbow_abi::BOOT_IMG_RM, b"rm".as_slice()),
                    (oxbow_abi::BOOT_IMG_MV, b"mv".as_slice()),
                    (oxbow_abi::BOOT_IMG_CP, b"cp".as_slice()),
                ] {
                    if let Some(idx) = image::find(iname) {
                        p.install(
                            handle,
                            object::HandleEntry {
                                obj: object::ObjectRef::Image(idx),
                                rights: spawn_rights,
                            badge: 0,
                            },
                        );
                    }
                }
                // The root-directory capability: a BADGED endpoint to the fs
                // server (badge = FS_ROOT). The badge is the unforgeable node id;
                // opening a file relative to it yields a fresh badged file cap.
                p.install(
                    oxbow_abi::BOOT_FS_ROOT,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP2),
                        rights: oxbow_abi::R_SEND | oxbow_abi::R_GRANT,
                        badge: oxbow_abi::FS_ROOT,
                    },
                );
            });
        }
        // The fs server owns the filesystem endpoint UNBADGED, with full rights:
        // R_RECV to serve, R_SEND+R_ATTENUATE to mint badged file caps, R_GRANT to
        // hand them back in OPEN replies. It is the root of filesystem authority.
        if cmd == b"fs" {
            // Map the tar initrd read-only (NX) into the fs address space at the
            // fixed vaddr the server parses from. The fs holds no Memory/IoPort
            // cap over it — it's a plain read-only mapping the kernel grants once.
            if let Some((phys, size)) = initrd {
                let pages = (size + 0xfff) / 0x1000;
                for i in 0..pages {
                    mm::vm::map_user_4k_in(
                        as_i,
                        oxbow_abi::FS_INITRD + i * 0x1000,
                        phys + i * 0x1000,
                        false,
                        false,
                    );
                }
                println!("[fs] initrd mapped: {} pages @ {:#x}", pages, oxbow_abi::FS_INITRD);
            }
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_EP,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP2),
                        rights: oxbow_abi::R_SEND
                            | oxbow_abi::R_RECV
                            | oxbow_abi::R_GRANT
                            | oxbow_abi::R_ATTENUATE,
                        badge: 0,
                    },
                );
            });
        }
        // The serial driver gets the COM1 IRQ line + the 16550 RX ports as
        // capabilities. The ports are R_IN ONLY — the kernel keeps exclusive
        // ownership of every UART config/TX register, so the driver can only read.
        if cmd == b"serial" {
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_SERIAL_IRQ,
                    object::HandleEntry {
                        obj: object::ObjectRef::Irq(4), // COM1 line
                        rights: oxbow_abi::R_BIND | oxbow_abi::R_ACK,
                    badge: 0,
                    },
                );
                p.install(
                    oxbow_abi::BOOT_SERIAL_RBR,
                    object::HandleEntry {
                        obj: object::ObjectRef::IoPort { base: 0x3F8, len: 1 },
                        rights: oxbow_abi::R_IN,
                    badge: 0,
                    },
                );
                p.install(
                    oxbow_abi::BOOT_SERIAL_LSR,
                    object::HandleEntry {
                        obj: object::ObjectRef::IoPort { base: 0x3FD, len: 1 },
                        rights: oxbow_abi::R_IN,
                    badge: 0,
                    },
                );
                // The serial driver forwards received bytes to the TTY endpoint.
                p.install(
                    oxbow_abi::BOOT_TTY,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP1),
                        rights: oxbow_abi::R_SEND,
                    badge: 0,
                    },
                );
            });
        }
        let tcb = thread::spawn_user(pid, as_i, entry, user_rsp);
        println!("[user] {} scheduled as tcb {} (ring 3, IF=1)", name, tcb);
    }

    // The boot thread becomes the idle thread and runs the scheduler forever.
    thread::run_idle();
}

/// Bare-metal panic handler: report to serial, then halt. There is no unwinding.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\n[PANIC] {}", info);
    arch::halt();
}
