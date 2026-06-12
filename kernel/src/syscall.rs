//! Arch-neutral syscall dispatch.
//!
//! The arch entry stub (arch/x86_64/syscall.rs) marshals registers and calls
//! here. v0 grows this `match` one phase at a time; Phase 7 adds the non-IPC
//! capability syscalls (console_write, attenuate, close) and upgrades exit. The
//! IPC syscalls (send/recv/call/reply) arrive in Phase 8.
use core::mem::size_of;
use oxbow_abi::{
    Handle, MsgBuf, SysError, SysResult, HANDLE_NULL, MSG_DATA_WORDS, MSG_HANDLES, R_ATTENUATE,
    R_GRANT, R_RECV, R_SEND, R_WRITE, SYS_ATTENUATE, SYS_CALL, SYS_CLOSE, SYS_CONSOLE_WRITE,
    SYS_EXIT, SYS_RECV, SYS_REPLY, SYS_SEND,
};

use crate::object::{HandleEntry, ObjType, ObjectRef};
use crate::{arch, ipc, println, proc, usermem};

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
    _a4: u64,
    _a5: u64,
    _a6: u64,
    nr: u64,
) -> SyscallRet {
    match nr {
        SYS_SEND => sys_ipc(a1, a2, false),
        SYS_RECV => sys_recv(a1, a2),
        SYS_CALL => sys_ipc(a1, a2, true),
        SYS_REPLY => sys_reply(a1, a2),
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
