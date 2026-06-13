//! Arch-neutral syscall dispatch.
//!
//! The arch entry stub (arch/x86_64/syscall.rs) marshals registers and calls
//! here. v0 grows this `match` one phase at a time; Phase 7 adds the non-IPC
//! capability syscalls (console_write, attenuate, close) and upgrades exit. The
//! IPC syscalls (send/recv/call/reply) arrive in Phase 8.
use core::mem::size_of;
use oxbow_abi::{
    Handle, MsgBuf, SysError, SysResult, BOOT_MEM, HANDLE_NULL, MSG_DATA_WORDS, MSG_HANDLES,
    PROT_READ, PROT_WRITE, R_ATTENUATE, R_GRANT, R_MAP, R_RECV, R_SEND, R_SIGNAL, R_WAIT, R_WRITE,
    SPAWN_DEFAULT_BUDGET, SPAWN_SLOTS, SYS_ATTENUATE, SYS_CALL, SYS_CLOSE, SYS_CONSOLE_WRITE,
    SYS_EXIT, SYS_FRAME_ALLOC, SYS_FRAME_MAP, SYS_IO_IN, SYS_IO_OUT, SYS_IRQ_ACK, SYS_IRQ_BIND,
    SYS_EP_CREATE, SYS_MAP, SYS_MINT, SYS_NOTIF_CREATE, SYS_NOTIF_SIGNAL, SYS_NOTIF_WAIT,
    SYS_DMA_ALLOC, SYS_PCI_BAR_MAP, SYS_PCI_READ, SYS_PCI_WRITE, SYS_RECV, SYS_REPLY, SYS_SEND,
    SYS_SPAWN, R_ACK,
    R_BIND, R_IN, R_OUT, R_SPAWN,
};

use crate::object::{HandleEntry, ObjType, ObjectRef};
use crate::{arch, ipc, mm, notif, println, proc, usermem};

/// One past the canonical lower half (the user range).
const LOWER_HALF_END: u64 = 0x0000_8000_0000_0000;

/// Max bytes per `sys_console_write` (ABI §4.3).
const CONSOLE_MAX: u64 = 1024;

/// The two syscall return values, in the SysV result registers: `rax` carries
/// `0`/`SysError`, `rdx` carries a freshly allocated `Handle` (or `HANDLE_NULL`).
#[repr(C)]
pub struct SyscallRet {
    pub rax: u64,
    pub rdx: u64,
}

impl SyscallRet {
    fn ok() -> Self {
        SyscallRet {
            rax: 0,
            rdx: HANDLE_NULL as u64,
        }
    }
    fn ok_handle(h: Handle) -> Self {
        SyscallRet {
            rax: 0,
            rdx: h as u64,
        }
    }
    fn err(e: SysError) -> Self {
        SyscallRet {
            rax: e as u64,
            rdx: HANDLE_NULL as u64,
        }
    }
    fn from_result(r: SysResult) -> Self {
        match r {
            Ok(()) => Self::ok(),
            Err(e) => Self::err(e),
        }
    }
}

/// Syscall dispatcher. `nr` arrives as the 7th (stack) argument so a1..a6 stay
/// in their SysV registers untouched by the entry stub. See ABI §4.3.
pub extern "C" fn syscall_dispatch(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    _a5: u64,
    _a6: u64,
    nr: u64,
) -> SyscallRet {
    match nr {
        SYS_SEND => sys_ipc(a1, a2, false),
        SYS_RECV => sys_recv(a1, a2),
        SYS_CALL => sys_ipc(a1, a2, true),
        SYS_REPLY => sys_reply(a1, a2),
        SYS_MAP => sys_map(a1, a2, a3, a4),
        SYS_FRAME_ALLOC => sys_frame_alloc(a1),
        SYS_FRAME_MAP => sys_frame_map(a1, a2, a3),
        SYS_NOTIF_CREATE => sys_notif_create(),
        SYS_NOTIF_SIGNAL => sys_notif_signal(a1),
        SYS_NOTIF_WAIT => sys_notif_wait(a1),
        SYS_IO_IN => sys_io_in(a1, a2),
        SYS_IO_OUT => sys_io_out(a1, a2, a3),
        SYS_IRQ_BIND => sys_irq_bind(a1, a2),
        SYS_IRQ_ACK => sys_irq_ack(a1),
        SYS_SPAWN => sys_spawn(a1, a2, a3, a4),
        SYS_EP_CREATE => sys_ep_create(),
        SYS_MINT => sys_mint(a1, a2, a3),
        SYS_PCI_READ => sys_pci_read(a1, a2),
        SYS_PCI_WRITE => sys_pci_write(a1, a2, a3),
        SYS_PCI_BAR_MAP => sys_pci_bar_map(a1, a2, a3),
        SYS_DMA_ALLOC => sys_dma_alloc(a1, a2),
        SYS_CONSOLE_WRITE => sys_console_write(a1, a2, a3),
        SYS_ATTENUATE => sys_attenuate(a1, a2),
        SYS_CLOSE => SyscallRet::from_result(proc::with_current_mut(|p| p.close(a1 as Handle))),
        SYS_EXIT => {
            proc::kill(crate::thread::current_proc()); // close handles, mark Dead
            println!("[proc] server exited ({})", a1);
            crate::thread::exit_current(); // kill this thread; the machine lives on
        }
        _ => SyscallRet::err(SysError::Nosys),
    }
}

/// Shared send/call path. Error order per ABI: handle → type → rights (on the
/// ep) → E_FAULT (align + page walk) → E_MSG (limits) → E_RIGHTS (R_GRANT per
/// transferred handle, §3.4) — ALL before any side effect.
fn sys_ipc(ep: u64, msg_ptr: u64, is_call: bool) -> SyscallRet {
    // Validate everything BEFORE any side effect; return the ep index + copy-in.
    let prepared = (|| -> SysResult<(u8, MsgBuf)> {
        let entry = proc::with_current(|p| p.lookup(ep as Handle, ObjType::Endpoint, R_SEND))?;
        let ObjectRef::Endpoint(idx) = entry.obj else {
            return Err(SysError::BadType);
        };
        // Pointer: 8-aligned + mapped. `call` needs write (reply overwrites it).
        if msg_ptr & 7 != 0 {
            return Err(SysError::Fault);
        }
        usermem::check_user(msg_ptr, size_of::<MsgBuf>(), is_call)?;
        let mut msg: MsgBuf = unsafe { core::ptr::read(msg_ptr as *const MsgBuf) };
        if msg.data_len as usize > MSG_DATA_WORDS || msg.handle_count as usize > MSG_HANDLES {
            return Err(SysError::Msg);
        }
        // Stamp the invoked cap's badge, OVERWRITING whatever the sender wrote —
        // this is what makes the delivered badge unforgeable (§14). Unbadged caps
        // carry badge 0, so an ordinary send delivers 0.
        msg.badge = entry.badge;
        // §3.4: every transferred handle needs R_GRANT in the sender's table.
        if msg.handle_count > 0 {
            proc::with_current(|p| -> SysResult {
                for &h in &msg.handles[..msg.handle_count as usize] {
                    if p.get(h)?.rights & R_GRANT == 0 {
                        return Err(SysError::Rights);
                    }
                }
                Ok(())
            })?;
        } // process lock dropped before delivery — never held with ENDPOINTS
        Ok((idx, msg))
    })();

    match prepared {
        Ok((idx, msg)) => {
            // All validation passed (no side effects yet). Rendezvous: may block.
            let (rax, rdx) = ipc::send_or_call(idx, &msg, msg_ptr, is_call);
            SyscallRet { rax, rdx }
        }
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_map(mem, vaddr, len, prot)` — map anonymous zeroed pages into the
/// caller's own address space, debiting the Memory budget `mem` (law L6). All
/// validation precedes any side effect; the map cannot partially fail.
fn sys_map(mem: u64, vaddr: u64, len: u64, prot: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        // 1. Capability: a Memory handle with R_MAP.
        let entry = proc::with_current(|p| p.lookup(mem as Handle, ObjType::Memory, R_MAP))?;
        let ObjectRef::Memory(midx) = entry.obj else {
            return Err(SysError::BadType);
        };
        // 2. Shape: 4 KiB-aligned vaddr+len, nonzero, valid prot (read implied).
        if vaddr & 0xfff != 0 || len == 0 || len & 0xfff != 0 {
            return Err(SysError::Msg);
        }
        if prot & !(PROT_READ | PROT_WRITE) != 0 || prot & PROT_READ == 0 {
            return Err(SysError::Msg);
        }
        let pages = len / 4096;
        // 3. Early budget bound (also caps the probe's work — DoS guard).
        if pages.saturating_mul(4096) > mm::mem::remaining(midx) {
            return Err(SysError::NoMem);
        }
        // 4. Range must be lower-half and not wrap.
        let end = vaddr.checked_add(len).ok_or(SysError::Fault)?;
        if end > LOWER_HALF_END {
            return Err(SysError::Fault);
        }
        // 5. Probe: overlap → E_FAULT; also the exact missing-table count.
        let pml4 = mm::vm::current_pml4();
        let missing = mm::vm::probe_user_range(pml4, vaddr, pages).map_err(|_| SysError::Fault)?;
        // 6. Charge pages + intermediate tables, up front, atomically.
        let cost = (pages + missing) * 4096;
        if !mm::mem::debit(midx, cost) {
            return Err(SysError::NoMem);
        }
        // 7. Map (infallible now: budget reserved, no page present, single CPU).
        let writable = prot & PROT_WRITE != 0;
        for p in 0..pages {
            let frame = mm::pmm::alloc_frame().expect("sys_map: PMM exhausted under budget");
            mm::vm::map_user_4k_live(pml4, vaddr + p * 4096, frame, writable);
        }
        println!(
            "[mem] proc {} map {} pages (+{} pt) @ {:#x} -> {} KiB left",
            crate::thread::current_proc(),
            pages,
            missing,
            vaddr,
            mm::mem::remaining(midx) / 1024
        );
        Ok(())
    })())
}

/// `sys_notif_create()` — mint a fresh Notification, returned as a handle.
fn sys_notif_create() -> SyscallRet {
    match notif::create() {
        Some(idx) => {
            let r = proc::with_current_mut(|p| {
                p.alloc_slot(HandleEntry {
                    obj: ObjectRef::Notification(idx),
                    rights: R_SIGNAL | R_WAIT | R_GRANT | R_ATTENUATE,
                badge: 0,
                })
            });
            match r {
                Ok(h) => SyscallRet::ok_handle(h),
                Err(e) => SyscallRet::err(e), // notif pool slot leaks (bounded)
            }
        }
        None => SyscallRet::err(SysError::NoMem),
    }
}

/// `sys_notif_signal(notif)` — bump a notification (requires R_SIGNAL).
fn sys_notif_signal(notif_h: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let entry =
            proc::with_current(|p| p.lookup(notif_h as Handle, ObjType::Notification, R_SIGNAL))?;
        let ObjectRef::Notification(idx) = entry.obj else {
            return Err(SysError::BadType);
        };
        notif::signal(idx);
        Ok(())
    })())
}

/// `sys_notif_wait(notif)` — block until signalled; returns the latched count in
/// rdx (requires R_WAIT).
fn sys_notif_wait(notif_h: u64) -> SyscallRet {
    let validated = (|| -> SysResult<u8> {
        let entry =
            proc::with_current(|p| p.lookup(notif_h as Handle, ObjType::Notification, R_WAIT))?;
        let ObjectRef::Notification(idx) = entry.obj else {
            return Err(SysError::BadType);
        };
        Ok(idx)
    })();
    match validated {
        Ok(idx) => {
            let (rax, rdx) = notif::wait(idx);
            SyscallRet { rax, rdx }
        }
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_irq_bind(irq, notif)` — route a hardware line to a notification. The
/// binder must hold R_BIND on the line and R_SIGNAL on the notification (binding
/// delegates signal authority to the kernel). Does not unmask — the first ack
/// arms the line.
fn sys_irq_bind(irq_h: u64, notif_h: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let irq = proc::with_current(|p| p.lookup(irq_h as Handle, ObjType::Irq, R_BIND))?;
        let ObjectRef::Irq(line) = irq.obj else {
            return Err(SysError::BadType);
        };
        let n = proc::with_current(|p| {
            p.lookup(notif_h as Handle, ObjType::Notification, R_SIGNAL)
        })?;
        let ObjectRef::Notification(nidx) = n.obj else {
            return Err(SysError::BadType);
        };
        crate::irq::bind(line, nidx);
        Ok(())
    })())
}

/// `sys_irq_ack(irq)` — re-arm (unmask) a bound line for the next interrupt
/// (requires R_ACK). Called by the driver after draining the device.
fn sys_irq_ack(irq_h: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let irq = proc::with_current(|p| p.lookup(irq_h as Handle, ObjType::Irq, R_ACK))?;
        let ObjectRef::Irq(line) = irq.obj else {
            return Err(SysError::BadType);
        };
        if !crate::irq::is_bound(line) {
            return Err(SysError::Msg);
        }
        crate::irq::ack(line);
        Ok(())
    })())
}

/// `sys_ep_create()` — mint a fresh Endpoint, returned as a full-rights handle.
/// Lets a parent set up an IPC channel between the children it spawns.
fn sys_ep_create() -> SyscallRet {
    match ipc::ep_create() {
        Some(idx) => {
            let r = proc::with_current_mut(|p| {
                p.alloc_slot(HandleEntry {
                    obj: ObjectRef::Endpoint(idx),
                    rights: R_SEND | R_RECV | R_GRANT | R_ATTENUATE,
                badge: 0,
                })
            });
            match r {
                Ok(h) => SyscallRet::ok_handle(h),
                Err(e) => SyscallRet::err(e), // ep pool slot leaks (bounded)
            }
        }
        None => SyscallRet::err(SysError::NoMem),
    }
}

/// Pages `load_into` will map for an image: per PT_LOAD, the page span from the
/// page containing p_vaddr to the page after p_vaddr+p_memsz.
fn spawn_load_pages(img: &crate::elf::Image) -> u64 {
    let mut pages = 0u64;
    for ph in img.loads() {
        let start = ph.p_vaddr & !0xfff;
        let end = (ph.p_vaddr + ph.p_memsz + 0xfff) & !0xfff;
        pages += (end - start) / 4096;
    }
    pages
}

/// `sys_spawn(image, mem, &MsgBuf, exit_notif)` — load a spawnable Image into a
/// fresh process, granting it the capabilities named in the spawn MsgBuf, and
/// start it. The parent's Memory budget pays for the child's frames + budget
/// (the seL4-honest model: spawning consumes the spawner's untyped). Returns the
/// child pid in rdx (informational — no authority). See ABI §13.
fn sys_spawn(image_h: u64, mem_h: u64, msg_ptr: u64, exit_notif_h: u64) -> SyscallRet {
    // 16 stack pages + a conservative page-table overhead (fresh PML4 + tables
    // for ≤2 mapped regions needs ≤7); mirrors proc::load_into's stack size.
    const STACK_PAGES: u64 = 16;
    const PT_OVERHEAD: u64 = 8;

    // ---- Validate everything; no side effects (handle→type→rights, then the
    // MsgBuf, then per-grant R_GRANT, then the image, then the budget bound). ----
    struct Prep {
        img_idx: u8,
        midx: u8,
        exit_idx: Option<u8>,
        grants: [Option<HandleEntry>; MSG_HANDLES],
        grant_count: usize,
        child_budget: u64,
        cost: u64,
        argv: [u8; 56],
        argv_len: usize,
    }
    let prep = (|| -> SysResult<Prep> {
        let ie = proc::with_current(|p| p.lookup(image_h as Handle, ObjType::Image, R_SPAWN))?;
        let ObjectRef::Image(img_idx) = ie.obj else {
            return Err(SysError::BadType);
        };
        let me = proc::with_current(|p| p.lookup(mem_h as Handle, ObjType::Memory, R_MAP))?;
        let ObjectRef::Memory(midx) = me.obj else {
            return Err(SysError::BadType);
        };
        let exit_idx = if exit_notif_h != HANDLE_NULL as u64 {
            let ne = proc::with_current(|p| {
                p.lookup(exit_notif_h as Handle, ObjType::Notification, R_SIGNAL)
            })?;
            let ObjectRef::Notification(nidx) = ne.obj else {
                return Err(SysError::BadType);
            };
            Some(nidx)
        } else {
            None
        };
        if msg_ptr & 7 != 0 {
            return Err(SysError::Fault);
        }
        usermem::check_user(msg_ptr, size_of::<MsgBuf>(), false)?;
        let msg: MsgBuf = unsafe { core::ptr::read(msg_ptr as *const MsgBuf) };
        let grant_count = msg.handle_count as usize;
        if grant_count > MSG_HANDLES || grant_count > SPAWN_SLOTS.len() {
            return Err(SysError::Msg);
        }
        let child_budget = if msg.data[0] == 0 { SPAWN_DEFAULT_BUDGET } else { msg.data[0] };
        // The argument string rides in data[1..] (bytes 8..64), NUL-terminated.
        let mut argv = [0u8; 56];
        let db = unsafe { core::slice::from_raw_parts(msg.data.as_ptr() as *const u8, 64) };
        argv.copy_from_slice(&db[8..64]);
        let argv_len = argv.iter().position(|&b| b == 0).unwrap_or(argv.len());
        // Collect the granted handle entries; each non-null needs R_GRANT (§3.4).
        let mut grants: [Option<HandleEntry>; MSG_HANDLES] = [None; MSG_HANDLES];
        proc::with_current(|p| -> SysResult {
            for i in 0..grant_count {
                let h = msg.handles[i];
                if h == HANDLE_NULL {
                    continue;
                }
                let e = p.get(h)?;
                if e.rights & R_GRANT == 0 {
                    return Err(SysError::Rights);
                }
                grants[i] = Some(e);
            }
            Ok(())
        })?;
        // Validate the image now (a bad image is an error, not a panic).
        let bytes = crate::image::bytes(img_idx).ok_or(SysError::Msg)?;
        let img = crate::elf::Image::try_validate(bytes)?;
        // +1 page for the argv page the kernel maps into the child (§13).
        let cost = (spawn_load_pages(&img) + STACK_PAGES + PT_OVERHEAD + 1) * 4096 + child_budget;
        // Authority bound: the parent must be able to afford it. We CHECK rather
        // than debit here so a later slot-full failure costs nothing; the kernel
        // is non-preemptible (IF=0, single CPU), so nothing allocates between the
        // check and the debit below.
        if mm::mem::remaining(midx) < cost {
            return Err(SysError::NoMem);
        }
        Ok(Prep {
            img_idx,
            midx,
            exit_idx,
            grants,
            grant_count,
            child_budget,
            cost,
            argv,
            argv_len,
        })
    })();
    let prep = match prep {
        Ok(p) => p,
        Err(e) => return SyscallRet::err(e),
    };

    // ---- Side effects. Create the child (claims a slot, loads the image), then
    // mint its budget, install its handles, debit the parent, and start it. ----
    let bytes = crate::image::bytes(prep.img_idx).expect("spawn: image vanished");
    let img = crate::elf::Image::try_validate(bytes).expect("spawn: image re-validate");
    let pml4 = mm::vm::new_user_pml4();
    let (cid, entry, rsp) = match proc::create(&img, pml4, "spawned") {
        Ok(t) => t,
        Err(e) => return SyscallRet::err(e), // pool full — nothing debited yet
    };
    let child_mem = match mm::mem::grant(prep.child_budget) {
        Some(m) => m,
        None => return SyscallRet::err(SysError::NoMem),
    };
    proc::with_proc_mut(cid, |p| {
        p.install(
            BOOT_MEM,
            HandleEntry {
                obj: ObjectRef::Memory(child_mem),
                rights: R_MAP | R_GRANT | R_ATTENUATE,
            badge: 0,
            },
        );
        for i in 0..prep.grant_count {
            if let Some(e) = prep.grants[i] {
                p.install(SPAWN_SLOTS[i], e);
            }
        }
    });
    // Record the lifecycle: the spawner (prep.midx) is refunded prep.cost when
    // this child dies and its frames are reclaimed (slot reuse).
    proc::set_lifecycle(cid, prep.exit_idx, child_mem, prep.midx, prep.cost);
    // Map the argv page (read-only) into the child at SPAWN_ARGV and write the
    // argument string there. Always mapped (empty string if no arg) so any child
    // can read it safely. Charged via the +1 page in `cost`.
    if let Some(argframe) = mm::pmm::alloc_frame() {
        unsafe {
            let dst = mm::phys_to_virt(argframe) as *mut u8;
            core::ptr::write_bytes(dst, 0, 4096);
            core::ptr::copy_nonoverlapping(prep.argv.as_ptr(), dst, prep.argv_len);
        }
        mm::vm::map_user_4k_in(pml4, oxbow_abi::SPAWN_ARGV, argframe, false, false);
    }
    // Debit the parent now (guaranteed to succeed — we checked `remaining`).
    let _ = mm::mem::debit(prep.midx, prep.cost);
    let tcb = crate::thread::spawn_user(cid, pml4, entry, rsp);
    println!("[spawn] pid {} (tcb {}) image#{} -{} KiB", cid, tcb, prep.img_idx, prep.cost / 1024);
    SyscallRet::ok_handle(cid as Handle)
}

/// `sys_io_in(ioport, port)` — read a byte from a port authorized by an IoPort
/// capability (requires R_IN). The byte is returned in rdx.
fn sys_io_in(io: u64, port: u64) -> SyscallRet {
    let r = (|| -> SysResult<u64> {
        let entry = proc::with_current(|p| p.lookup(io as Handle, ObjType::IoPort, R_IN))?;
        let ObjectRef::IoPort { base, len } = entry.obj else {
            return Err(SysError::BadType);
        };
        let port = port as u16;
        if port < base || port >= base + len {
            return Err(SysError::Msg);
        }
        Ok(arch::io_in(port) as u64)
    })();
    match r {
        Ok(v) => SyscallRet { rax: 0, rdx: v },
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_io_out(ioport, port, value)` — write a byte to a port (requires R_OUT).
fn sys_io_out(io: u64, port: u64, value: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let entry = proc::with_current(|p| p.lookup(io as Handle, ObjType::IoPort, R_OUT))?;
        let ObjectRef::IoPort { base, len } = entry.obj else {
            return Err(SysError::BadType);
        };
        let port = port as u16;
        if port < base || port >= base + len {
            return Err(SysError::Msg);
        }
        arch::io_out(port, value as u8);
        Ok(())
    })())
}

/// `sys_frame_alloc(mem)` — debit one frame from `mem` and mint a Frame object
/// (a nameable, mappable, shareable physical frame), returned as a handle.
fn sys_frame_alloc(mem: u64) -> SyscallRet {
    let result = (|| -> SysResult<Handle> {
        let entry = proc::with_current(|p| p.lookup(mem as Handle, ObjType::Memory, R_MAP))?;
        let ObjectRef::Memory(midx) = entry.obj else {
            return Err(SysError::BadType);
        };
        if !mm::mem::debit(midx, 4096) {
            return Err(SysError::NoMem);
        }
        let phys = mm::pmm::alloc_frame().ok_or(SysError::NoMem)?;
        let fidx = mm::mem::frame_record(phys).ok_or(SysError::NoMem)?;
        proc::with_current_mut(|p| {
            p.alloc_slot(HandleEntry {
                obj: ObjectRef::Frame(fidx),
                rights: R_MAP | R_WRITE | R_GRANT | R_ATTENUATE,
            badge: 0,
            })
        })
    })();
    match result {
        Ok(h) => SyscallRet::ok_handle(h),
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_frame_map(frame, vaddr, prot)` — map a specific Frame into the caller's
/// AS. PROT_WRITE requires R_WRITE on the HANDLE (not the object) — so an
/// attenuated read-only handle yields read-only shared memory (the §3.4 payoff).
fn sys_frame_map(frame: u64, vaddr: u64, prot: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let entry = proc::with_current(|p| p.lookup(frame as Handle, ObjType::Frame, R_MAP))?;
        let ObjectRef::Frame(fidx) = entry.obj else {
            return Err(SysError::BadType);
        };
        if vaddr & 0xfff != 0 {
            return Err(SysError::Msg);
        }
        if prot & !(PROT_READ | PROT_WRITE) != 0 || prot & PROT_READ == 0 {
            return Err(SysError::Msg);
        }
        let writable = prot & PROT_WRITE != 0;
        if writable && entry.rights & R_WRITE == 0 {
            return Err(SysError::Rights); // read-only handle can't map writable
        }
        if vaddr >= LOWER_HALF_END {
            return Err(SysError::Fault);
        }
        let pml4 = mm::vm::current_pml4();
        mm::vm::probe_user_range(pml4, vaddr, 1).map_err(|_| SysError::Fault)?;
        // Map the SAME physical frame (intermediate tables uncharged in v1).
        let phys = mm::mem::frame_phys(fidx);
        mm::vm::map_user_4k_live(pml4, vaddr, phys, writable);
        mm::mem::frame_inc_map(fidx); // refcount the mapping (§9 reclamation)
        Ok(())
    })())
}

/// Receive on an endpoint we hold with R_RECV. Blocks until a sender arrives;
/// returns a Reply handle (in rdx) for a call, or HANDLE_NULL for a plain send.
fn sys_recv(ep: u64, msg_ptr: u64) -> SyscallRet {
    let validated = (|| -> SysResult<u8> {
        let entry = proc::with_current(|p| p.lookup(ep as Handle, ObjType::Endpoint, R_RECV))?;
        let ObjectRef::Endpoint(idx) = entry.obj else {
            return Err(SysError::BadType);
        };
        if msg_ptr & 7 != 0 {
            return Err(SysError::Fault);
        }
        usermem::check_user(msg_ptr, size_of::<MsgBuf>(), true)?;
        Ok(idx)
    })();
    match validated {
        Ok(idx) => {
            let (rax, rdx) = ipc::recv(idx, msg_ptr);
            SyscallRet { rax, rdx }
        }
        Err(e) => SyscallRet::err(e),
    }
}

/// Reply to a pending call via a one-shot Reply handle. Consumes the handle on
/// success (frees the pool slot + our table slot); not consumed on validation
/// errors (ABI §4.3). Never blocks.
fn sys_reply(reply: u64, msg_ptr: u64) -> SyscallRet {
    let prepared = (|| -> SysResult<(u8, MsgBuf)> {
        let entry = proc::with_current(|p| p.lookup(reply as Handle, ObjType::Reply, 0))?;
        let ObjectRef::Reply(idx) = entry.obj else {
            return Err(SysError::BadType);
        };
        if msg_ptr & 7 != 0 {
            return Err(SysError::Fault);
        }
        usermem::check_user(msg_ptr, size_of::<MsgBuf>(), false)?;
        let mut m: MsgBuf = unsafe { core::ptr::read(msg_ptr as *const MsgBuf) };
        if m.data_len as usize > MSG_DATA_WORDS || m.handle_count as usize > MSG_HANDLES {
            return Err(SysError::Msg);
        }
        // §3.4 also governs reply-carried handles: each needs R_GRANT in the
        // replier's table (lets a server hand a freshly-minted cap back in OPEN).
        if m.handle_count > 0 {
            proc::with_current(|p| -> SysResult {
                for &h in &m.handles[..m.handle_count as usize] {
                    if p.get(h)?.rights & R_GRANT == 0 {
                        return Err(SysError::Rights);
                    }
                }
                Ok(())
            })?;
        }
        m.badge = 0; // a reply always delivers badge 0 (§14): badges are forward-only
        Ok((idx, m))
    })();
    match prepared {
        Ok((idx, m)) => {
            ipc::do_reply(idx as usize, &m); // copies to caller staging, wakes it
            // Consume the Reply handle from our own table (success path, §4.3).
            let _ = proc::with_current_mut(|p| p.close(reply as Handle));
            SyscallRet::ok()
        }
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_console_write(con, buf, len)` — write user bytes through a Console cap.
fn sys_console_write(con: u64, buf: u64, len: u64) -> SyscallRet {
    let result = (|| -> SysResult {
        // Capability check (lock dropped before the side effect).
        proc::with_current(|p| p.lookup(con as Handle, ObjType::Console, R_WRITE))?;
        if len > CONSOLE_MAX {
            return Err(SysError::Msg);
        }
        usermem::check_user(buf, len as usize, false)?;
        let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
        arch::console_write_bytes(slice);
        Ok(())
    })();
    SyscallRet::from_result(result)
}

/// `sys_mint(src, badge, new_rights)` — derive a BADGED capability to the same
/// endpoint (§14). The badge is delivered, unforgeably, to whoever receives a
/// message sent through the derived cap. Rules: `src` must be an unbadged
/// Endpoint held with `R_ATTENUATE`; `new_rights` must be a subset (law L5);
/// `badge != 0`. The source's badge being 0 is mandatory — re-badging is
/// forbidden, which is exactly what makes a delivered badge unforgeable.
fn sys_mint(src: u64, badge: u64, new_rights: u64) -> SyscallRet {
    let new_rights = new_rights as u32;
    proc::with_current_mut(|p| {
        let entry = match p.get(src as Handle) {
            Ok(e) => e,
            Err(e) => return SyscallRet::err(e),
        };
        if entry.obj.ty() != ObjType::Endpoint {
            return SyscallRet::err(SysError::BadType);
        }
        if entry.rights & R_ATTENUATE == 0 {
            return SyscallRet::err(SysError::Rights);
        }
        if entry.badge != 0 {
            return SyscallRet::err(SysError::Rights); // no re-badging (immutable)
        }
        if new_rights & entry.rights != new_rights {
            return SyscallRet::err(SysError::Rights); // no amplification (L5)
        }
        if badge == 0 {
            return SyscallRet::err(SysError::Msg); // 0 stays "unbadged"
        }
        match p.alloc_slot(HandleEntry {
            obj: entry.obj,
            rights: new_rights,
            badge,
        }) {
            Ok(h) => SyscallRet::ok_handle(h),
            Err(e) => SyscallRet::err(e),
        }
    })
}

/// Decode a PciDevice cap (with the required right) into (bus, dev, func).
fn pci_dev(handle: u64, right: u32) -> SysResult<(u8, u8, u8)> {
    let e = proc::with_current(|p| p.lookup(handle as Handle, ObjType::PciDevice, right))?;
    let ObjectRef::PciDevice(bdf) = e.obj else {
        return Err(SysError::BadType);
    };
    Ok(((bdf >> 16) as u8, (bdf >> 8) as u8, bdf as u8))
}

/// `sys_pci_read(pcidev, offset) -> u32` — read a config-space register of the
/// device this capability names (requires R_IN). The value is returned in rdx.
fn sys_pci_read(dev: u64, offset: u64) -> SyscallRet {
    match pci_dev(dev, R_IN) {
        Ok((b, d, f)) => SyscallRet {
            rax: 0,
            rdx: crate::pci::config_read(b, d, f, offset as u8) as u64,
        },
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_pci_write(pcidev, offset, value)` — write a config-space register
/// (requires R_OUT). Used e.g. to enable bus-mastering / MMIO decode.
fn sys_pci_write(dev: u64, offset: u64, value: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let (b, d, f) = pci_dev(dev, R_OUT)?;
        crate::pci::config_write(b, d, f, offset as u8, value as u32);
        Ok(())
    })())
}

/// `sys_pci_bar_map(pcidev, bar, vaddr)` — map the device's memory BAR `bar`
/// (uncacheable) into the caller's address space at `vaddr` (requires R_MAP). The
/// kernel reads the BAR's physical base + size from config space; this is the
/// only way a driver reaches its device registers.
fn sys_pci_bar_map(dev: u64, bar: u64, vaddr: u64) -> SyscallRet {
    SyscallRet::from_result((|| -> SysResult {
        let (b, d, f) = pci_dev(dev, R_MAP)?;
        if vaddr & 0xfff != 0 || vaddr >= LOWER_HALF_END {
            return Err(SysError::Fault);
        }
        let device = crate::pci::Device {
            bus: b,
            dev: d,
            func: f,
            vendor: 0,
            device: 0,
            class: 0,
            subclass: 0,
        };
        let (base, size) = device.bar_region(bar as u8);
        if base == 0 || size == 0 || size > 0x10_0000 {
            return Err(SysError::Msg); // not a memory BAR, or implausibly large
        }
        let pml4 = mm::vm::current_pml4();
        let pages = (size + 0xfff) / 0x1000;
        for i in 0..pages {
            mm::vm::map_mmio_4k_in(pml4, vaddr + i * 0x1000, base + i * 0x1000);
        }
        Ok(())
    })())
}

/// `sys_dma_alloc(mem, vaddr) -> phys` — allocate one frame, map it writable
/// (cacheable) into the caller's AS at `vaddr`, and return its physical address
/// in rdx. A bus-mastering driver needs known physical addresses to program a
/// device's ring-base registers and descriptor buffer pointers. Paid from the
/// Memory budget (R_MAP). The frame is an ordinary lower-half mapping, so AS
/// teardown (§16) reclaims it like any other; no IOMMU exists in v0, so a driver
/// holding a bus-master device cap could already DMA anywhere — exposing the
/// physical address of its own frames adds no authority it lacked.
fn sys_dma_alloc(mem: u64, vaddr: u64) -> SyscallRet {
    let result = (|| -> SysResult<u64> {
        let entry = proc::with_current(|p| p.lookup(mem as Handle, ObjType::Memory, R_MAP))?;
        let ObjectRef::Memory(midx) = entry.obj else {
            return Err(SysError::BadType);
        };
        if vaddr & 0xfff != 0 || vaddr >= LOWER_HALF_END {
            return Err(SysError::Fault);
        }
        if !mm::mem::debit(midx, 4096) {
            return Err(SysError::NoMem);
        }
        let phys = mm::pmm::alloc_frame().ok_or(SysError::NoMem)?;
        let pml4 = mm::vm::current_pml4();
        mm::vm::probe_user_range(pml4, vaddr, 1).map_err(|_| SysError::Fault)?;
        mm::vm::map_user_4k_live(pml4, vaddr, phys, true);
        Ok(phys)
    })();
    match result {
        Ok(phys) => SyscallRet { rax: 0, rdx: phys },
        Err(e) => SyscallRet::err(e),
    }
}

/// `sys_attenuate(src, new_rights)` — derive a strictly-weaker handle (law L5).
fn sys_attenuate(src: u64, new_rights: u64) -> SyscallRet {
    let new_rights = new_rights as u32;
    proc::with_current_mut(|p| {
        let entry = match p.get(src as Handle) {
            Ok(e) => e,
            Err(e) => return SyscallRet::err(e),
        };
        // Reply objects are not attenuable (ABI §2.2).
        if entry.obj.ty() == ObjType::Reply {
            return SyscallRet::err(SysError::BadType);
        }
        if entry.rights & R_ATTENUATE == 0 {
            return SyscallRet::err(SysError::Rights);
        }
        // New rights must be a subset of the source's (no amplification).
        if new_rights & entry.rights != new_rights {
            return SyscallRet::err(SysError::Rights);
        }
        match p.alloc_slot(HandleEntry {
            obj: entry.obj,
            rights: new_rights,
            badge: entry.badge, // attenuation preserves the badge (§14)
        }) {
            Ok(h) => SyscallRet::ok_handle(h),
            Err(e) => SyscallRet::err(e),
        }
    })
}
