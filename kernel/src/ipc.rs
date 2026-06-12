//! Phase 8 — synchronous endpoint IPC, v0 model.
//!
//! v0 has ONE user thread and the kernel echo is permanently armed on EP0 (the
//! embodiment of ABI §7 step 5's "kernel-internal receive"). A sender therefore
//! never waits: the whole call → receive → reply completes inline on the
//! caller's kernel stack. Rendezvous (L7) still holds — both parties are
//! present — so v0 needs no wait queue, no context save, no scheduler.
//!
//! INVARIANT: `sys_call` returns 0 ⟺ `do_reply` wrote the caller's buffer.
//!
//! LOCK RULE: `PROCESS` and `ENDPOINTS` are never held simultaneously; receivers
//! and `do_reply` run lock-free (page walk + user write only).
use core::mem::size_of;
use oxbow_abi::{MsgBuf, SysError, SysResult, MSG_DATA_WORDS, MSG_HANDLES, TAG_PING, TAG_PONG};
use spin::Mutex;

use crate::usermem;

/// Endpoint pool index of the boot endpoint EP0 (`ObjectRef::Endpoint(EP0)`).
pub const EP0: u8 = 0;
const EP_POOL: usize = 8; // static pool, law L6

/// A kernel-mode receiver permanently armed on an endpoint. Contract: for a call
/// (`token` is `Some`) it must either consume the token via `do_reply` and
/// return its result, or drop the token and return `Err` (reply abandoned → the
/// caller sees `E_GONE`). For a send (`token` is `None`), `Ok(())` = delivered.
type KernelReceiver = fn(&MsgBuf, Option<ReplyToken>) -> SysResult;

#[derive(Clone, Copy)]
struct Endpoint {
    in_use: bool,
    receiver: Option<KernelReceiver>,
}

static ENDPOINTS: Mutex<[Endpoint; EP_POOL]> = Mutex::new(
    [Endpoint {
        in_use: false,
        receiver: None,
    }; EP_POOL],
);

/// One-shot authority to answer a pending call. Deliberately NOT Clone/Copy and
/// consumed by value: move semantics enforce the Reply invariant (§2.2). v0
/// keeps it inline (the call is fully synchronous, so nothing outlives it); the
/// pooled Reply object + `ObjectRef::Reply(u8)` is the v1 user-receiver path.
pub struct ReplyToken {
    caller_msg_uptr: u64,
}

/// Boot-time: create EP0 and arm the kernel echo on it (ABI §7 steps 2 & 5).
pub fn init() {
    ENDPOINTS.lock()[EP0 as usize] = Endpoint {
        in_use: true,
        receiver: Some(echo),
    };
    crate::println!("[ipc] EP0 created; kernel echo armed (receive side)");
}

/// Deliver a call. `msg` is the dispatcher-validated copy-in; `caller_msg_uptr`
/// is the caller's (validated, writable, 8-aligned) buffer for the reply.
pub fn do_call(ep_idx: u8, msg: &MsgBuf, caller_msg_uptr: u64) -> SysResult {
    let recv = lookup_receiver(ep_idx)?; // ENDPOINTS lock dropped before delivery
    recv(msg, Some(ReplyToken { caller_msg_uptr }))
}

/// Deliver a one-way send: no reply token, complete at rendezvous.
pub fn do_send(ep_idx: u8, msg: &MsgBuf) -> SysResult {
    let recv = lookup_receiver(ep_idx)?;
    recv(msg, None)
}

fn lookup_receiver(ep_idx: u8) -> Result<KernelReceiver, SysError> {
    let eps = ENDPOINTS.lock();
    let ep = eps.get(ep_idx as usize).ok_or(SysError::Gone)?;
    if !ep.in_use {
        return Err(SysError::Gone); // endpoint destroyed (unreachable in v0)
    }
    ep.receiver.ok_or(SysError::Gone)
} // guard drops here; the receiver runs lock-free

/// Write a reply into the calling thread's buffer, consuming the token. Shared
/// by the kernel echo now and user `sys_reply` in v1. Writes ONLY the valid
/// prefix — §5: unused trailing words/slots are left unmodified.
pub fn do_reply(token: ReplyToken, reply: &MsgBuf) -> SysResult {
    let dl = reply.data_len as usize;
    let hc = reply.handle_count as usize;
    if dl > MSG_DATA_WORDS || hc > MSG_HANDLES {
        return Err(SysError::Msg); // guards a buggy kernel receiver
    }
    // Cheap revalidation; load-bearing once callers can block across a reply.
    usermem::check_user(token.caller_msg_uptr, size_of::<MsgBuf>(), true)?;
    let dst = token.caller_msg_uptr as *mut MsgBuf;
    unsafe {
        // ptr is 8-aligned (dispatcher-checked), so each field write is aligned.
        (&raw mut (*dst).tag).write(reply.tag);
        (&raw mut (*dst).data_len).write(reply.data_len);
        (&raw mut (*dst).handle_count).write(reply.handle_count);
        for i in 0..dl {
            (&raw mut (*dst).data[i]).write(reply.data[i]);
        }
        for i in 0..hc {
            (&raw mut (*dst).handles[i]).write(reply.handles[i]);
        }
    }
    Ok(())
}

/// The v0 echo parent (ABI §7 step 7): PING → PONG; anything else → reply
/// abandoned (token dropped) → the caller's `sys_call` returns `E_GONE`.
fn echo(msg: &MsgBuf, token: Option<ReplyToken>) -> SysResult {
    match token {
        None => Ok(()), // one-way send: accepted, nothing owed
        Some(t) => {
            if msg.tag != TAG_PING {
                return Err(SysError::Gone); // t dropped = reply abandoned
            }
            let mut r = MsgBuf::new(TAG_PONG);
            r.data_len = 1;
            r.data[0] = u64::from_le_bytes(*b"PONG\n\0\0\0");
            do_reply(t, &r)
        }
    }
}
