//! oxbow microkernel — milestone-0 ("first light").
//!
//! Boots via Limine, brings up the serial console, prints a banner, and halts.
//! This is deliberately ABI-neutral: it proves the toolchain -> boot -> QEMU
//! loop works before any of the capability machinery in docs/abi-v0.md exists.
#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

mod arch;
mod channel;
mod elf;
mod fb;
mod image;
mod ipc;
mod irq;
mod mm;
mod notif;
mod object;
mod percpu;
mod pci;
mod pipe;
mod proc;
mod shm;
mod rng;
mod smp;
mod syscall;
mod thread;
mod usermem;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    MpRequest, RequestsEndMarker, RequestsStartMarker,
};
use limine::BaseRevision;

/// Boot chatter gate: the ELF/spawn/mem traces are useful while bringing the
/// system up, but they would spam the console on every shell-launched command.
/// On during boot, switched off just before the scheduler takes over.
static BOOT_VERBOSE: AtomicBool = AtomicBool::new(true);
pub fn verbose() -> bool {
    BOOT_VERBOSE.load(Ordering::Relaxed)
}

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

/// Ask Limine for a linear framebuffer (the foundation for graphics: the fb
/// server will own this region as a capability and composite into it).
#[used]
#[link_section = ".requests"]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

/// §69 SMP — ask Limine to bring up the Application Processors (APs). Limine
/// parses the ACPI MADT, starts each AP, and parks it; we get a CPU list (LAPIC
/// IDs) and a `goto_address` per CPU to launch it at a Rust entry with its own
/// stack — no hand-rolled trampoline or INIT-SIPI-SIPI. Phase 1 only enumerates;
/// bringup comes later once per-CPU state + locking are in place.
#[used]
#[link_section = ".requests"]
static MP_REQUEST: MpRequest = MpRequest::new();

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

    // §69 SMP Phase 1: enumerate the CPUs Limine brought up (the APs are parked,
    // waiting for a goto_address). We don't start them yet — this just proves the
    // topology is visible before we build per-CPU state + locking.
    if let Some(mp) = MP_REQUEST.get_response() {
        let cpus = mp.cpus();
        println!(
            "[smp] {} CPU(s); BSP lapic_id={}",
            cpus.len(),
            mp.bsp_lapic_id()
        );
        for cpu in cpus {
            let role = if cpu.lapic_id == mp.bsp_lapic_id() { "BSP" } else { "AP " };
            println!("[smp]   cpu id={} lapic_id={} ({}, parked)", cpu.id, cpu.lapic_id, role);
        }
    } else {
        println!("[smp] no MP response (single CPU / no Limine MP)");
    }

    // §69 LAPIC enable happens in kmain_stage2, AFTER the switch to the kernel's
    // own PML4 — otherwise the LAPIC MMIO mapping lands in the soon-discarded
    // stage-1 page tables and the timer faults once stage 2 is running.

    // -- Framebuffer: record geometry + paint a smoke-test pattern -----------
    if let Some(fb) = FRAMEBUFFER_REQUEST.get_response().and_then(|r| r.framebuffers().next()) {
        fb::init(fb.addr() as u64, fb.width() as u32, fb.height() as u32, fb.pitch() as u32, fb.bpp());
        println!(
            "[fb] {}x{} {}bpp pitch={} @ {:#x}",
            fb.width(),
            fb.height(),
            fb.bpp(),
            fb.pitch(),
            fb.addr() as u64
        );
        fb::test_pattern();
    } else {
        println!("[fb] no framebuffer from Limine");
    }

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

    // §69 Phase 4: bring up this (BSP) CPU's per-CPU state and point its GS base at
    // it BEFORE anything reads `thread::current()` — the running-thread id now lives
    // in PerCpu (gs:[8]), not a global.
    percpu::init(0);

    // The boot thread becomes the idle thread (TCB 0).
    thread::init();

    // -- v1 arc 2 Phase 1: address-space construction primitive --------------
    // Prove a second PML4 (sharing the kernel upper half) can be hopped into and
    // back. Runs BEFORE the timer is armed, so IF=0 guarantees no trap mid-hop.
    mm::vm::as_hop_selftest();

    // §69 Phase 2a: enable the BSP's LAPIC (virtual-wire) now that the kernel's
    // own PML4 is active — the LAPIC MMIO mapping must live in the page tables
    // user processes copy their higher half from (every interrupt touches it).
    let lapic_id = arch::lapic::enable();
    println!("[smp] BSP LAPIC enabled (virtual-wire), id={}", lapic_id);

    // §69 Phase 2c: route the ISA device IRQs (keyboard 1, serial 4, mouse 12)
    // through the IOAPIC to the BSP's LAPIC, on the same vectors their handlers
    // already use (PIC-remap base 0x20). Those PIC lines stay masked, so each IRQ
    // arrives once — through the IOAPIC. The drivers unmask via irq::ack, which now
    // re-arms the IOAPIC for these lines. PCI IRQs (the NIC) keep using the PIC's
    // virtual wire until PCI INTx→GSI routing lands.
    arch::ioapic::init();
    for (gsi, vector) in [(1u8, 0x21u8), (4, 0x24), (12, 0x2C)] {
        arch::ioapic::route(gsi, vector, lapic_id as u8);
    }
    println!("[smp] IOAPIC routing kbd/mouse/serial -> BSP lapic {}", lapic_id);

    // §69 Phase 2b: the scheduler tick is the LAPIC timer (calibrated against PIT
    // channel 2), NOT the legacy PIT IRQ0 — IRQ0 stays masked. Keyboard/mouse/serial
    // IRQs still reach the CPU through the PIC's virtual-wire LINT0 (set up in 2a).
    arch::lapic::start_timer(arch::lapic::TIMER_VECTOR, 100);

    // §72/§74: bring up EVERY available Application Processor into the scheduler.
    // Each AP runs the shared run queue under SCHED_LOCK, so user threads are
    // load-balanced across all cores. Safe now that the lost-wakeup protocol (§70),
    // the context-switch handoff (§71), per-CPU syscall stacks (§72), and the
    // lock-ordering audit (§73) are all in place.
    if let Some(mp) = MP_REQUEST.get_response() {
        smp::bring_up_all(mp);
    }

    // Seed the CSPRNG before any process loads — stack-base ASLR draws from it.
    rng::init();

    ipc::init();

    // Enumerate the PCI bus — the kernel is the root of hardware authority and
    // will hand a future net driver a capability to the NIC it finds here.
    let (nic, blk) = pci::enumerate();
    match nic {
        Some(d) => {
            let (base, size) = d.bar_region(0);
            println!(
                "[pci] NIC {:04x}:{:04x} at {:02x}:{:02x}.{} — BAR0 {:#x} ({} KiB) IRQ {}",
                d.vendor, d.device, d.bus, d.dev, d.func, base, size / 1024, d.irq_line()
            );
        }
        None => println!("[pci] no network controller found"),
    }
    match blk {
        Some(d) => {
            let raw = d.bar(0);
            let io_base = (raw & 0xFFFC) as u16; // legacy virtio: I/O-port BAR
            println!(
                "[pci] virtio-blk {:04x}:{:04x} at {:02x}:{:02x}.{} — I/O port {:#06x} IRQ {}",
                d.vendor, d.device, d.bus, d.dev, d.func, io_base, d.irq_line()
            );
        }
        None => println!("[pci] no virtio-blk device found"),
    }

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

    // §47: a kernel-created channel carrying keyboard events from the `kbd` driver
    // (side 0, send) to the `oxcomp` compositor (side 1, receive). Both ends are
    // installed at BOOT_INPUT_CHAN as the modules come up below.
    let input_chan = channel::create();

    // §53: a channel mirroring the tty's output to the graphical terminal — the
    // tty (side 0) writes its console output, oxterm (side 1, via the compositor)
    // reads it and renders it. So the login/shell text appears on screen.
    let term_chan = channel::create();

    // §54: a channel carrying PS/2 mouse packets from the kbd/i8042 driver
    // (side 0) to the compositor (side 1), which moves the cursor.
    let mouse_chan = channel::create();

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
            b"pong" | b"beta" | b"hello" | b"badge" | b"cat" | b"ls" | b"mkdir" | b"touch" | b"rm" | b"mv" | b"cp" | b"drift" | b"cc-hello" | b"tcc" | b"lua" | b"micropython" | b"qjs" | b"curl" | b"cares-test" | b"ffi-test" | b"wl-test" | b"xkb-test" | b"vterm-test" | b"ft-test" | b"wlclient" | b"oxterm" | b"sysmon" | b"jail" | b"fstest"
        ) {
            image::register(cmd, bytes);
            println!("[mod] image '{}' registered ({} bytes)", name, bytes.len());
            continue;
        }

        println!("[mod] module '{}': {} bytes", name, bytes.len());
        let img = elf::Image::validate(bytes);
        let as_i = mm::vm::new_user_pml4();
        let (pid, entry, user_rsp) = proc::create(&img, as_i, name).expect("boot: create");
        // §24: map a zeroed identity page so boot modules read as root via
        // rt::identity() (runtime spawns get theirs in spawn_common). Without this
        // the first read of SPAWN_IDENT faults.
        if let Some(idframe) = mm::pmm::alloc_frame() {
            unsafe {
                core::ptr::write_bytes(mm::phys_to_virt(idframe) as *mut u8, 0, 0x1000);
            }
            mm::vm::map_user_4k_in(as_i, oxbow_abi::SPAWN_IDENT, idframe, false, false);
        }
        // The shell is the system's spawner; it needs a large budget to fund the
        // processes it launches (it pays from its own untyped). Drivers get 256K.
        // The shell funds every program it spawns from its own budget; tcc wants
        // a large working set, so the shell needs plenty of headroom. The fs
        // server maps an 8 MiB file-storage arena, so it gets 16 MiB.
        let budget = if cmd == b"shell" {
            96 * 1024 * 1024
        } else if cmd == b"fs" {
            16 * 1024 * 1024
        } else if cmd == b"oxcomp" {
            108 * 1024 * 1024 // libwayland + shm + funds the oxterm/rings/sysmon children
        } else {
            mm::mem::BOOT_BUDGET
        };
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
                // §47: the SEND end of the keyboard channel to the compositor.
                if let Some(conn) = input_chan {
                    p.install(
                        oxbow_abi::BOOT_INPUT_CHAN,
                        object::HandleEntry {
                            obj: object::ObjectRef::Channel { conn, side: 0 },
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT,
                        badge: 0,
                        },
                    );
                }
                // §54: the PS/2 mouse IRQ line (12) + the SEND end of the mouse
                // packet channel. The kbd driver owns the shared i8042, so it
                // also services the mouse.
                p.install(
                    oxbow_abi::BOOT_MOUSE_IRQ,
                    object::HandleEntry {
                        obj: object::ObjectRef::Irq(12),
                        rights: oxbow_abi::R_BIND
                            | oxbow_abi::R_ACK
                            | oxbow_abi::R_GRANT
                            | oxbow_abi::R_ATTENUATE,
                        badge: 0,
                    },
                );
                if let Some(conn) = mouse_chan {
                    p.install(
                        oxbow_abi::BOOT_MOUSE_CHAN,
                        object::HandleEntry {
                            obj: object::ObjectRef::Channel { conn, side: 0 },
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT,
                            badge: 0,
                        },
                    );
                }
            });
        }
        // The fb server / oxcomp compositor get the framebuffer capability — the
        // sole holder of the authority to map + draw to the screen.
        if cmd == b"fb" || cmd == b"oxcomp" {
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_FB,
                    object::HandleEntry {
                        obj: object::ObjectRef::Framebuffer,
                        rights: oxbow_abi::R_MAP | oxbow_abi::R_GRANT | oxbow_abi::R_ATTENUATE,
                    badge: 0,
                    },
                )
            });
        }
        // §47: the compositor gets the RECEIVE end of the keyboard channel; its
        // event loop watches this fd and turns key bytes into wl_keyboard events.
        // §53: and the RECEIVE end of the terminal-mirror channel, which it passes
        // on to oxterm at spawn so the shell's output renders in the window.
        if cmd == b"oxcomp" {
            proc::with_proc_mut(pid, |p| {
                if let Some(conn) = input_chan {
                    p.install(
                        oxbow_abi::BOOT_INPUT_CHAN,
                        object::HandleEntry {
                            obj: object::ObjectRef::Channel { conn, side: 1 },
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT,
                            badge: 0,
                        },
                    );
                }
                if let Some(conn) = term_chan {
                    p.install(
                        oxbow_abi::BOOT_TERM_CHAN,
                        object::HandleEntry {
                            obj: object::ObjectRef::Channel { conn, side: 1 },
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT | oxbow_abi::R_GRANT,
                            badge: 0,
                        },
                    );
                }
                // §54: the RECEIVE end of the mouse-packet channel — the compositor
                // moves the cursor and emits wl_pointer from these.
                if let Some(conn) = mouse_chan {
                    p.install(
                        oxbow_abi::BOOT_MOUSE_CHAN,
                        object::HandleEntry {
                            obj: object::ObjectRef::Channel { conn, side: 1 },
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT,
                            badge: 0,
                        },
                    );
                }
            });
        }
        // The compositor also gets a spawnable Image cap for its Wayland client,
        // so it can launch wlclient and hand it a channel (wlclient's module must
        // precede oxcomp's so it is already registered).
        if cmd == b"oxcomp" {
            // The compositor's client is the terminal (§52); the wlclient rings
            // demo image is still granted so it can be spawned for testing.
            for (name, handle) in [
                (b"wlclient".as_slice(), oxbow_abi::BOOT_IMG_WLCLIENT),
                (b"oxterm".as_slice(), oxbow_abi::BOOT_IMG_OXTERM),
                (b"sysmon".as_slice(), oxbow_abi::BOOT_IMG_SYSMON),
            ] {
                if let Some(idx) = image::find(name) {
                    proc::with_proc_mut(pid, |p| {
                        p.install(
                            handle,
                            object::HandleEntry {
                                obj: object::ObjectRef::Image(idx),
                                rights: oxbow_abi::R_SPAWN
                                    | oxbow_abi::R_GRANT
                                    | oxbow_abi::R_ATTENUATE,
                                badge: 0,
                            },
                        )
                    });
                } else {
                    println!("[boot] WARN: client image not found for oxcomp");
                }
            }
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
                );
                // §53: the SEND end of the terminal-mirror channel — the tty
                // forwards its console output here for oxterm to render.
                if let Some(conn) = term_chan {
                    p.install(
                        oxbow_abi::BOOT_TERM_CHAN,
                        object::HandleEntry {
                            obj: object::ObjectRef::Channel { conn, side: 0 },
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT,
                            badge: 0,
                        },
                    );
                }
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
                    (oxbow_abi::BOOT_IMG_DRIFT, b"drift".as_slice()),
                    (oxbow_abi::BOOT_IMG_CCHELLO, b"cc-hello".as_slice()),
                    (oxbow_abi::BOOT_IMG_TCC, b"tcc".as_slice()),
                    (oxbow_abi::BOOT_IMG_LUA, b"lua".as_slice()),
                    (oxbow_abi::BOOT_IMG_UPY, b"micropython".as_slice()),
                    (oxbow_abi::BOOT_IMG_QJS, b"qjs".as_slice()),
                    (oxbow_abi::BOOT_IMG_CURL, b"curl".as_slice()),
                    (oxbow_abi::BOOT_IMG_JAIL, b"jail".as_slice()),
                    (oxbow_abi::BOOT_IMG_FSTEST, b"fstest".as_slice()),
                    (oxbow_abi::BOOT_IMG_CARES, b"cares-test".as_slice()),
                    (oxbow_abi::BOOT_IMG_FFI, b"ffi-test".as_slice()),
                    (oxbow_abi::BOOT_IMG_WL, b"wl-test".as_slice()),
                    (oxbow_abi::BOOT_IMG_XKB, b"xkb-test".as_slice()),
                    (oxbow_abi::BOOT_IMG_VTERM, b"vterm-test".as_slice()),
                    (oxbow_abi::BOOT_IMG_FT, b"ft-test".as_slice()),
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
                // The network control capability: a BADGED endpoint to the net
                // server (badge = NET_CTL). `udp_bind` on it mints a fresh badged
                // UDP-socket cap — the network analogue of BOOT_FS_ROOT.
                p.install(
                    oxbow_abi::BOOT_NET_EP,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP3),
                        rights: oxbow_abi::R_SEND | oxbow_abi::R_GRANT,
                        badge: oxbow_abi::NET_CTL,
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
                // A SEND cap to the block service (EP4): the fs persists its
                // writable files to disk through this, and restores them at boot.
                // The only authority over storage the fs holds — it owns no PCI or
                // DMA cap of its own.
                p.install(
                    oxbow_abi::BOOT_BLK_EP,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP4),
                        rights: oxbow_abi::R_SEND,
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
        // The net driver gets a capability to the ONE NIC the kernel found:
        // config-space read/write + the authority to map its MMIO BARs.
        if cmd == b"net" {
            if let Some(d) = nic {
                proc::with_proc_mut(pid, |p| {
                    p.install(
                        oxbow_abi::BOOT_PCI,
                        object::HandleEntry {
                            obj: object::ObjectRef::PciDevice(d.bdf()),
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT | oxbow_abi::R_MAP,
                            badge: 0,
                        },
                    );
                    // ...and a capability to that NIC's interrupt line, so it can
                    // bind the IRQ to a notification and ack it (mask/unmask).
                    p.install(
                        oxbow_abi::BOOT_NET_IRQ,
                        object::HandleEntry {
                            obj: object::ObjectRef::Irq(d.irq_line()),
                            rights: oxbow_abi::R_BIND | oxbow_abi::R_ACK,
                            badge: 0,
                        },
                    );
                });
            }
            // The net server owns the network endpoint UNBADGED with full rights
            // (the root of network authority): R_RECV to serve socket requests,
            // R_SEND+R_ATTENUATE to mint badged socket caps, R_GRANT to hand them
            // back in bind replies. Installed even without a NIC so clients fail
            // cleanly rather than block forever.
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_EP,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP3),
                        rights: oxbow_abi::R_SEND
                            | oxbow_abi::R_RECV
                            | oxbow_abi::R_GRANT
                            | oxbow_abi::R_ATTENUATE,
                        badge: 0,
                    },
                );
            });
        }
        if cmd == b"blk" {
            // The block driver owns the virtio-blk PCI device: read/write config
            // (R_IN|R_OUT) to negotiate + map its MMIO BAR (R_MAP). DMA comes from
            // its standard Memory budget.
            if let Some(d) = blk {
                proc::with_proc_mut(pid, |p| {
                    p.install(
                        oxbow_abi::BOOT_PCI,
                        object::HandleEntry {
                            obj: object::ObjectRef::PciDevice(d.bdf()),
                            rights: oxbow_abi::R_IN | oxbow_abi::R_OUT | oxbow_abi::R_MAP,
                            badge: 0,
                        },
                    );
                });
            }
            // (see below: fsd — the lwext4 fs server — gets a SEND cap to EP4.)
            // It also owns the block-service endpoint UNBADGED with full rights
            // (the root of block authority): R_RECV to serve sector requests.
            // Installed even without a disk so the fs server's calls fail cleanly
            // (degraded loop) rather than block forever.
            proc::with_proc_mut(pid, |p| {
                p.install(
                    oxbow_abi::BOOT_EP,
                    object::HandleEntry {
                        obj: object::ObjectRef::Endpoint(ipc::EP4),
                        rights: oxbow_abi::R_SEND | oxbow_abi::R_RECV | oxbow_abi::R_GRANT,
                        badge: 0,
                    },
                );
            });
        }
        let tcb = thread::spawn_user(pid, as_i, entry, user_rsp);
        println!("[user] {} scheduled as tcb {} (ring 3, IF=1)", name, tcb);
    }

    // Boot is done; silence the per-spawn ELF/mem traces so shell commands run
    // cleanly (a Unix shell doesn't narrate every exec).
    BOOT_VERBOSE.store(false, Ordering::Relaxed);

    // The boot thread becomes the idle thread and runs the scheduler forever.
    thread::run_idle();
}

/// Set the instant any CPU starts panicking, so the NMI handler on the other cores
/// knows to halt (§75 panic stop). SeqCst — it gates real cross-core safety.
pub static PANICKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// §75 DEBUG: each NMI-stopped core records the rip it was interrupted at here, so
/// the core that triggered the stop can print where every core was wedged.
pub static STOPPED_RIP: [core::sync::atomic::AtomicU64; 8] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 8];

/// §75 DEBUG: a spinlock-timeout watchdog detected a likely deadlock — stop every
/// other core (NMI), then print this core's complaint plus every stopped core's rip
/// (recorded by their NMI handlers). Turns a silent IF=0 multi-core hang into a
/// readable "who was stuck where".
pub fn deadlock_report(lock: &str, waiter: i32, holder: i32) -> ! {
    use core::sync::atomic::Ordering;
    if !PANICKED.swap(true, Ordering::SeqCst) {
        unsafe { arch::lapic::send_nmi_all_but_self() };
        for _ in 0..8_000_000 {
            core::hint::spin_loop(); // let the others take the NMI + record their rip
        }
    }
    arch::panic_print(format_args!(
        "\n[DEADLOCK] cpu {} stuck waiting on {} (held by cpu {})\n",
        waiter, lock, holder
    ));
    for c in 0..8 {
        let rip = STOPPED_RIP[c].load(Ordering::Acquire);
        if rip != 0 {
            arch::panic_print(format_args!("  cpu {} was at rip={:#x}\n", c, rip));
        }
    }
    arch::halt();
}

/// Bare-metal panic handler. §75: on SMP a fault used to hang silently — the
/// faulting core's print deadlocked on the console lock another core held. Now we
/// (1) flag the panic, (2) NMI every other core so it halts in the NMI handler
/// (FreeBSD `stop_cpus_hard` — NMI reaches cores spinning IF=0 or wedged in a
/// handler), (3) print via the lock-bypassing console, then halt. So a multi-core
/// fault produces a readable oops instead of a freeze.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // If we re-enter (a fault while panicking), just halt — don't recurse.
    if PANICKED.swap(true, Ordering::SeqCst) {
        arch::halt();
    }
    unsafe { arch::lapic::send_nmi_all_but_self() };
    // Give the other cores a moment to take the NMI and halt before we print.
    for _ in 0..2_000_000 {
        core::hint::spin_loop();
    }
    arch::panic_print(format_args!("\n[PANIC cpu {}] {}\n", percpu::cpu_index(), info));
    arch::halt();
}
