//! Arch-neutral syscall dispatch.
//!
//! The arch entry stub (arch/x86_64/syscall.rs) marshals registers and calls
//! here. v0 grows this `match` one phase at a time; Phase 7 adds the non-IPC
//! capability syscalls (console_write, attenuate, close) and upgrades exit. The
//! IPC syscalls (send/recv/call/reply) arrive in Phase 8.
use core::mem::size_of;
use oxbow_abi::{
    Handle, MsgBuf, SysError, SysResult, HANDLE_NULL, MSG_DATA_WORDS, MSG_HANDLES, PROT_READ,
    PROT_WRITE, R_ATTENUATE, R_GRANT, R_MAP, R_RECV, R_SEND, R_SIGNAL, R_WAIT, R_WRITE,
    SYS_ATTENUATE, SYS_CALL, SYS_CLOSE, SYS_CONSOLE_WRITE, SYS_EXIT, SYS_FRAME_ALLOC, SYS_FRAME_MAP,
    SYS_IO_IN, SYS_IO_OUT, SYS_IRQ_ACK, SYS_IRQ_BIND, SYS_MAP, SYS_NOTIF_CREATE, SYS_NOTIF_SIGNAL,
    SYS_NOTIF_WAIT, SYS_RECV, SYS_REPLY, SYS_SEND, R_ACK, R_BIND, R_IN, R_OUT,
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
        let msg: MsgBuf = unsafe { core::ptr::read(msg_ptr as *const MsgBuf) };
        if msg.data_len as usize > MSG_DATA_WORDS || msg.handle_count as usize > MSG_HANDLES {
            return Err(SysError::Msg);
        }
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
        let m: MsgBuf = unsafe { core::ptr::read(msg_ptr as *const MsgBuf) };
        if m.data_len as usize > MSG_DATA_WORDS || m.handle_count as usize > MSG_HANDLES {
            return Err(SysError::Msg);
        }
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
        }) {
            Ok(h) => SyscallRet::ok_handle(h),
            Err(e) => SyscallRet::err(e),
        }
    })
}
