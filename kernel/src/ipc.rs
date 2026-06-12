//! Synchronous user-to-user IPC: blocking rendezvous over endpoints (v1 arc 3).
//!
//! The kernel echo is gone — a real user process receives and replies. A sender
//! that arrives before a receiver BLOCKS on the endpoint's send queue; a receiver
//! that arrives first blocks as the endpoint's `recv_waiter`. Whoever completes
//! the rendezvous wakes the other.
//!
//! CROSS-ADDRESS-SPACE RULE: a running thread only ever touches its OWN user
//! memory (under its own CR3). Messages destined for a blocked thread park in
//! that thread's kernel STAGING slot (in `.bss`, reachable from any CR3); the
//! blocked thread copies staging → its own user buffer when it resumes.
//!
//! LOCK RULE: never hold `ENDPOINTS`/`REPLIES` across `block_current` (a spin
//! lock held across a switch hangs). IF=0 single-CPU makes every lock window
//! contention-free; the only hazard is a lock held across a switch.
use core::mem::size_of;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};
use oxbow_abi::{Handle, MsgBuf, SysError, SysResult, HANDLE_NULL, MSG_DATA_WORDS, MSG_HANDLES};
use spin::Mutex;

use crate::object::{HandleEntry, ObjectRef};
use crate::thread::{self, MAX_THREADS};
use crate::{println, proc, usermem};

/// Endpoint pool index of the boot endpoint EP0.
pub const EP0: u8 = 0;
/// The TTY endpoint (kbd/shell → tty).
pub const EP1: u8 = 1;
const EP_POOL: usize = 8;
const REPLY_POOL: usize = 8;

// --- Endpoint wait queues -------------------------------------------------

/// FIFO of blocked sender thread ids. Capacity MAX_THREADS can never overflow
/// (≤ MAX_THREADS threads exist).
#[derive(Clone, Copy)]
struct WaitQ {
    q: [usize; MAX_THREADS],
    head: usize,
    len: usize,
}

impl WaitQ {
    const fn new() -> Self {
        WaitQ {
            q: [0; MAX_THREADS],
            head: 0,
            len: 0,
        }
    }
    fn push(&mut self, tid: usize) {
        debug_assert!(self.len < MAX_THREADS);
        let tail = (self.head + self.len) % MAX_THREADS;
        self.q[tail] = tid;
        self.len += 1;
    }
    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let tid = self.q[self.head];
        self.head = (self.head + 1) % MAX_THREADS;
        self.len -= 1;
        Some(tid)
    }
}

#[derive(Clone, Copy)]
struct Endpoint {
    in_use: bool,
    send_q: WaitQ,
    recv_waiter: Option<usize>, // one receiver per EP in arc 3
}

static ENDPOINTS: Mutex<[Endpoint; EP_POOL]> = Mutex::new(
    [Endpoint {
        in_use: false,
        send_q: WaitQ::new(),
        recv_waiter: None,
    }; EP_POOL],
);

// --- Per-thread IPC staging (kernel .bss, visible from every CR3) ---------

#[derive(Clone, Copy)]
struct IpcSlot {
    staging: MsgBuf,
    msg_uptr: u64, // the thread's OWN user MsgBuf*, validated before blocking
    is_call: bool, // meaningful while queued in a send_q
    copy_out: bool, // resume epilogue must copy staging -> msg_uptr
    ret_rax: u64,
    ret_rdx: u64,
}

static mut IPC_SLOTS: [IpcSlot; MAX_THREADS] = [IpcSlot {
    staging: MsgBuf::new(0),
    msg_uptr: 0,
    is_call: false,
    copy_out: false,
    ret_rax: 0,
    ret_rdx: 0,
}; MAX_THREADS];

fn slot(tid: usize) -> *mut IpcSlot {
    unsafe { addr_of_mut!(IPC_SLOTS[tid]) }
}

/// Deposit a syscall return for a blocked thread (used by `notif::signal` — no
/// message, no copy-out, just the `(rax, rdx)` pair).
pub(crate) fn deposit_ret(tid: usize, rax: u64, rdx: u64) {
    unsafe {
        (*slot(tid)).copy_out = false;
        (*slot(tid)).ret_rax = rax;
        (*slot(tid)).ret_rdx = rdx;
    }
}

/// Read the deposited `(rax, rdx)` on resume.
pub(crate) fn take_ret(tid: usize) -> (u64, u64) {
    unsafe { ((*slot(tid)).ret_rax, (*slot(tid)).ret_rdx) }
}

// --- Reply pool -----------------------------------------------------------

#[derive(Clone, Copy)]
struct Reply {
    in_use: bool,
    caller_tid: usize,
}

static REPLIES: Mutex<[Reply; REPLY_POOL]> = Mutex::new([Reply {
    in_use: false,
    caller_tid: 0,
}; REPLY_POOL]);

/// Allocate a Reply pool slot recording the caller thread; `None` if exhausted.
fn alloc_reply(caller_tid: usize) -> Option<usize> {
    let mut replies = REPLIES.lock();
    for i in 0..REPLY_POOL {
        if !replies[i].in_use {
            replies[i] = Reply {
                in_use: true,
                caller_tid,
            };
            return Some(i);
        }
    }
    None
}

fn free_reply(idx: usize) {
    REPLIES.lock()[idx].in_use = false;
}

fn reply_caller(idx: usize) -> usize {
    REPLIES.lock()[idx].caller_tid
}

// --- Message copy ---------------------------------------------------------

/// Write the valid prefix of `m` into the user MsgBuf at `uptr` (§5: trailing
/// words/slots left unmodified). Runs under the CALLING thread's CR3.
fn copy_msg_to_user(uptr: u64, m: &MsgBuf) -> SysResult {
    let dl = m.data_len as usize;
    let hc = m.handle_count as usize;
    if dl > MSG_DATA_WORDS || hc > MSG_HANDLES {
        return Err(SysError::Msg);
    }
    usermem::check_user(uptr, size_of::<MsgBuf>(), true)?;
    let dst = uptr as *mut MsgBuf;
    unsafe {
        (&raw mut (*dst).tag).write(m.tag);
        (&raw mut (*dst).data_len).write(m.data_len);
        (&raw mut (*dst).handle_count).write(m.handle_count);
        for i in 0..dl {
            (&raw mut (*dst).data[i]).write(m.data[i]);
        }
        for i in 0..hc {
            (&raw mut (*dst).handles[i]).write(m.handles[i]);
        }
    }
    Ok(())
}

/// On resume from a block: copy staging → our own user buffer if asked, then
/// return the pending `(rax, rdx)` our waker deposited.
fn resume_epilogue(me: usize) -> (u64, u64) {
    let s = slot(me);
    unsafe {
        if (*s).copy_out {
            (*s).copy_out = false;
            if let Err(e) = copy_msg_to_user((*s).msg_uptr, &(*s).staging) {
                return (e as u64, HANDLE_NULL as u64);
            }
        }
        ((*s).ret_rax, (*s).ret_rdx)
    }
}

// --- One-shot rendezvous-path announcements (proof both orderings happen) --
fn announce(done: &AtomicBool, msg: &str) {
    if !done.swap(true, Ordering::Relaxed) {
        println!("[ipc] rendezvous: {}", msg);
    }
}
static SENDER_FIRST: AtomicBool = AtomicBool::new(false);
static RECEIVER_FIRST: AtomicBool = AtomicBool::new(false);

// --- Boot -----------------------------------------------------------------

/// Create EP0 (user↔user PONG) and EP1 (the TTY endpoint).
pub fn init() {
    let mut eps = ENDPOINTS.lock();
    eps[EP0 as usize].in_use = true;
    eps[EP1 as usize].in_use = true;
    drop(eps);
    println!("[ipc] EP0 + EP1 created");
}

// --- The rendezvous -------------------------------------------------------

/// Mint a Reply for `caller` and install it (rights 0) in process `to_proc`'s
/// table. Returns `(table_handle, pool_idx)` or an error (caller restores state).
fn mint_reply(caller: usize, to_proc: usize) -> Result<(Handle, usize), SysError> {
    let idx = alloc_reply(caller).ok_or(SysError::NoMem)?;
    match proc::with_proc_mut(to_proc, |p| {
        p.alloc_slot(HandleEntry {
            obj: ObjectRef::Reply(idx as u8),
            rights: 0,
        })
    }) {
        Ok(h) => Ok((h, idx)),
        Err(e) => {
            free_reply(idx);
            Err(e)
        }
    }
}

/// Move the granted handles in `staging_tid`'s staged message from `src_proc`'s
/// table into `dst_proc`'s table (§3.4: a COPY — same rights, sender retains),
/// rewriting the staged indices to the receiver's. Validated R_GRANT already.
fn transfer_into(src_proc: usize, dst_proc: usize, staging_tid: usize) -> SysResult {
    let n = unsafe { (*slot(staging_tid)).staging.handle_count as usize };
    for i in 0..n {
        let src_h = unsafe { (*slot(staging_tid)).staging.handles[i] };
        let entry = proc::with_proc_mut(src_proc, |p| p.get(src_h))?;
        let new_h = proc::with_proc_mut(dst_proc, |p| p.alloc_slot(entry))?;
        unsafe { (*slot(staging_tid)).staging.handles[i] = new_h };
    }
    Ok(())
}

/// `sys_send`/`sys_call`: `msg` is the dispatcher's validated copy-in (read under
/// our CR3). Returns the final `(rax, rdx)`.
pub fn send_or_call(ep_idx: u8, msg: &MsgBuf, msg_uptr: u64, is_call: bool) -> (u64, u64) {
    let me = thread::current();

    let mut eps = ENDPOINTS.lock();
    let ep = &mut eps[ep_idx as usize];
    if !ep.in_use {
        return (SysError::Gone as u64, HANDLE_NULL as u64);
    }

    if let Some(r) = ep.recv_waiter.take() {
        // ---- receiver-first: a receiver is blocked waiting for us ----
        drop(eps);
        announce(&RECEIVER_FIRST, "receiver-first");
        // Deposit our message into the receiver's staging, then move any granted
        // handles into the receiver's table (rewriting the staged indices).
        unsafe { (*slot(r)).staging = *msg };
        if let Err(e) = transfer_into(thread::current_proc(), thread::process_of(r), r) {
            ENDPOINTS.lock()[ep_idx as usize].recv_waiter = Some(r);
            return (e as u64, HANDLE_NULL as u64);
        }
        let reply_h = if is_call {
            match mint_reply(me, thread::process_of(r)) {
                Ok((h, _idx)) => h,
                Err(e) => {
                    ENDPOINTS.lock()[ep_idx as usize].recv_waiter = Some(r);
                    return (e as u64, HANDLE_NULL as u64);
                }
            }
        } else {
            HANDLE_NULL
        };
        unsafe {
            (*slot(r)).copy_out = true;
            (*slot(r)).ret_rax = 0;
            (*slot(r)).ret_rdx = reply_h as u64;
        }
        thread::wake(r);

        if is_call {
            unsafe { (*slot(me)).msg_uptr = msg_uptr };
            thread::block_current(); // wait for sys_reply
            resume_epilogue(me)
        } else {
            (0, HANDLE_NULL as u64) // one-way send delivered
        }
    } else {
        // ---- sender-first: no receiver yet, stage ourselves and block ----
        unsafe {
            (*slot(me)).staging = *msg;
            (*slot(me)).msg_uptr = msg_uptr;
            (*slot(me)).is_call = is_call;
            (*slot(me)).copy_out = false;
        }
        ep.send_q.push(me);
        drop(eps);
        thread::block_current(); // woken by a receiver (send) or sys_reply (call)
        resume_epilogue(me)
    }
}

/// `sys_recv`: block until a sender rendezvouses. Returns `(rax, rdx)` with the
/// Reply handle (for a call) or HANDLE_NULL (plain send) in rdx.
pub fn recv(ep_idx: u8, msg_uptr: u64) -> (u64, u64) {
    let me = thread::current();

    loop {
        let mut eps = ENDPOINTS.lock();
        let ep = &mut eps[ep_idx as usize];
        if !ep.in_use {
            return (SysError::Gone as u64, HANDLE_NULL as u64);
        }

        if let Some(s) = ep.send_q.pop() {
            // ---- sender-first: a sender is queued ----
            announce(&SENDER_FIRST, "sender-first");
            let is_call = unsafe { (*slot(s)).is_call };
            // Move any granted handles from the sender's table into ours, rewriting
            // the staged indices to our table's (we are the running receiver).
            if let Err(e) = transfer_into(thread::process_of(s), thread::current_proc(), s) {
                unsafe {
                    (*slot(s)).ret_rax = e as u64;
                    (*slot(s)).ret_rdx = HANDLE_NULL as u64;
                    (*slot(s)).copy_out = false;
                }
                thread::wake(s);
                continue;
            }
            let reply_rdx = if is_call {
                match mint_reply(s, thread::current_proc()) {
                    Ok((h, _idx)) => h, // sender stays Blocked, awaiting reply
                    Err(e) => {
                        // can't serve this sender; wake it with the error, try next
                        unsafe {
                            (*slot(s)).ret_rax = e as u64;
                            (*slot(s)).ret_rdx = HANDLE_NULL as u64;
                            (*slot(s)).copy_out = false;
                        }
                        thread::wake(s);
                        continue;
                    }
                }
            } else {
                unsafe {
                    (*slot(s)).ret_rax = 0;
                    (*slot(s)).ret_rdx = HANDLE_NULL as u64;
                    (*slot(s)).copy_out = false;
                }
                thread::wake(s);
                HANDLE_NULL
            };
            drop(eps);
            // Copy the sender's staged message into OUR user buffer (our CR3).
            let staged = unsafe { (*slot(s)).staging };
            return match copy_msg_to_user(msg_uptr, &staged) {
                Ok(()) => (0, reply_rdx as u64),
                Err(e) => (e as u64, HANDLE_NULL as u64),
            };
        }

        // ---- no sender: block as the endpoint's receiver ----
        if ep.recv_waiter.is_some() {
            drop(eps);
            return (SysError::NoMem as u64, HANDLE_NULL as u64); // one receiver per EP
        }
        ep.recv_waiter = Some(me);
        unsafe { (*slot(me)).msg_uptr = msg_uptr };
        drop(eps);
        thread::block_current(); // woken by a sender depositing into our staging
        return resume_epilogue(me);
    }
}

/// `sys_reply`: deliver `reply` to the caller behind Reply pool slot `reply_idx`,
/// consuming the Reply. Never blocks.
pub fn do_reply(reply_idx: usize, reply: &MsgBuf) {
    let caller = reply_caller(reply_idx);
    unsafe {
        (*slot(caller)).staging = *reply;
        (*slot(caller)).copy_out = true;
        (*slot(caller)).ret_rax = 0;
        (*slot(caller)).ret_rdx = HANDLE_NULL as u64;
    }
    free_reply(reply_idx);
    thread::wake(caller);
}

/// Abandon a pending Reply (its handle was closed, or its holder died): wake the
/// blocked caller with `E_GONE` and an untouched buffer (§4.3).
pub fn reply_abandon(reply_idx: usize) {
    let caller = reply_caller(reply_idx);
    unsafe {
        (*slot(caller)).copy_out = false; // leave the caller's buffer untouched
        (*slot(caller)).ret_rax = SysError::Gone as u64;
        (*slot(caller)).ret_rdx = HANDLE_NULL as u64;
    }
    free_reply(reply_idx);
    thread::wake(caller);
}
