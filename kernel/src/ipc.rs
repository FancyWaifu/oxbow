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
use oxbow_abi::{Handle, MsgBuf, SysError, SysResult, HANDLE_NULL, MSG_DATA_WORDS, MSG_HANDLES, R_GRANT};
use crate::sync::DiagMutex;

use crate::object::{HandleEntry, ObjectRef};
use crate::thread::{self, MAX_THREADS};
use crate::{notif, println, proc, usermem};

/// Endpoint pool index of the boot endpoint EP0.
pub const EP0: u8 = 0;
/// The TTY endpoint (kbd/shell → tty).
pub const EP1: u8 = 1;
/// The filesystem endpoint (shell/clients → fs); badged per open file (§15).
pub const EP2: u8 = 2;
/// The network endpoint (clients → net); badged per socket / NET_CTL (§21).
pub const EP3: u8 = 3;
/// The block endpoint (fs → blk); unbadged sector read/write service (§24).
pub const EP4: u8 = 4;
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

static ENDPOINTS: DiagMutex<[Endpoint; EP_POOL]> = DiagMutex::new("ENDPOINTS",
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
    /// SNAPSHOT of the sender's granted handle ENTRIES, captured at send time. The
    /// staged MsgBuf only holds handle INDICES, and for a sender-first send the actual
    /// transfer happens later (when a receiver arrives) — re-resolving those indices
    /// then is a TOCTOU: a sibling thread (same table) can swap the slot meanwhile and
    /// smuggle a different / non-grantable cap. We freeze the validated entries here so
    /// the later transfer installs exactly what was sent and authorized.
    staged_entries: [Option<HandleEntry>; MSG_HANDLES],
    msg_uptr: u64, // the thread's OWN user MsgBuf*, validated before blocking
    is_call: bool, // meaningful while queued in a send_q
    copy_out: bool, // resume epilogue must copy staging -> msg_uptr
    ret_rax: u64,
    ret_rdx: u64,
}

static mut IPC_SLOTS: [IpcSlot; MAX_THREADS] = [IpcSlot {
    staging: MsgBuf::new(0),
    staged_entries: [None; MSG_HANDLES],
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
    /// Generation, bumped every time the slot is freed (reply delivered, or the caller
    /// died and the reply was abandoned). A Reply HANDLE records the generation it was
    /// minted at (in its badge); a stale handle to a freed/reused slot — e.g. a receiver
    /// that held a reply while the original caller died and the slot was reused for a NEW
    /// caller — is rejected, so it can't misdirect a reply to the wrong client. Same
    /// reclaimable-pool pattern as Frame/Memory.
    gen: u32,
}

static REPLIES: DiagMutex<[Reply; REPLY_POOL]> = DiagMutex::new("REPLIES", [Reply {
    in_use: false,
    caller_tid: 0,
    gen: 0,
}; REPLY_POOL]);

/// Allocate a Reply pool slot recording the caller thread; returns `(idx, gen)` or
/// `None` if exhausted. The generation is preserved across reuse (only bumped on free).
fn alloc_reply(caller_tid: usize) -> Option<(usize, u32)> {
    let mut replies = REPLIES.lock();
    for i in 0..REPLY_POOL {
        if !replies[i].in_use {
            let gen = replies[i].gen;
            replies[i] = Reply { in_use: true, caller_tid, gen };
            return Some((i, gen));
        }
    }
    None
}

fn free_reply(idx: usize) {
    let mut r = REPLIES.lock();
    r[idx].in_use = false;
    r[idx].gen = r[idx].gen.wrapping_add(1); // invalidate any stale handles to this slot
}

/// True iff Reply slot `idx` is live AND at generation `gen` (the one the caller's
/// handle was minted at). The use-after-free / wrong-caller guard for `take_reply`.
pub fn reply_gen_ok(idx: usize, gen: u32) -> bool {
    if idx >= REPLY_POOL {
        return false;
    }
    let r = REPLIES.lock();
    r[idx].in_use && r[idx].gen == gen
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
        // The badge of the cap the sender invoked, stamped by sys_ipc — delivered
        // unforgeably to the receiver (§14). This is the single delivery path, so
        // both rendezvous orderings get it.
        (&raw mut (*dst).badge).write(m.badge);
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

// --- Boot -----------------------------------------------------------------

/// Create EP0 (user↔user PONG) and EP1 (the TTY endpoint).
pub fn init() {
    let mut eps = ENDPOINTS.lock();
    eps[EP0 as usize].in_use = true;
    eps[EP1 as usize].in_use = true;
    eps[EP2 as usize].in_use = true;
    eps[EP3 as usize].in_use = true;
    eps[EP4 as usize].in_use = true;
    drop(eps);
    println!("[ipc] EP0 + EP1 + EP2 + EP3 + EP4 created");
}

/// Mint a fresh endpoint from the pool (for `sys_ep_create`); returns its pool
/// index, or `None` if the pool is exhausted. (No reclamation in v1 — the pool
/// is bounded and a long-lived shell mints only a couple.)
pub fn ep_create() -> Option<u8> {
    let mut eps = ENDPOINTS.lock();
    for i in 0..EP_POOL {
        if !eps[i].in_use {
            eps[i] = Endpoint {
                in_use: true,
                send_q: WaitQ::new(),
                recv_waiter: None,
            };
            return Some(i as u8);
        }
    }
    None
}

// --- The rendezvous -------------------------------------------------------

/// Mint a Reply for `caller` and install it (rights 0) in process `to_proc`'s
/// table. Returns `(table_handle, pool_idx)` or an error (caller restores state).
fn mint_reply(caller: usize, to_proc: usize) -> Result<(Handle, usize), SysError> {
    let (idx, gen) = alloc_reply(caller).ok_or(SysError::NoMem)?;
    match proc::with_proc_mut(to_proc, |p| {
        p.alloc_slot(HandleEntry {
            obj: ObjectRef::Reply(idx as u8),
            rights: 0,
            badge: gen as u64, // stamp the generation; take_reply verifies it
        })
    }) {
        Ok(h) => Ok((h, idx)),
        Err(e) => {
            free_reply(idx);
            Err(e)
        }
    }
}

/// Snapshot the sender's granted handle ENTRIES into `staging_tid`'s slot, resolving
/// each staged index against `src_proc` and requiring R_GRANT. Called the instant the
/// staging MsgBuf is set (in the sender's own non-preemptible syscall window), so the
/// frozen entries can't be swapped before the later transfer (closes the TOCTOU). On
/// any invalid / non-grantable handle the whole send is rejected.
fn stage_entries(staging_tid: usize, src_proc: usize, msg: &MsgBuf) -> SysResult {
    let n = msg.handle_count as usize;
    for i in 0..n {
        let entry = proc::with_proc_mut(src_proc, |p| p.get(msg.handles[i]))?;
        if entry.rights & R_GRANT == 0 {
            return Err(SysError::Rights);
        }
        unsafe { (*slot(staging_tid)).staged_entries[i] = Some(entry) };
    }
    Ok(())
}

/// Move the granted handles from `staging_tid`'s slot into `dst_proc`'s table (§3.4: a
/// COPY — same rights, sender retains), rewriting the staged indices to the receiver's.
///
/// Two complementary checks defeat a sibling thread racing the sender's shared table
/// between stage and transfer (a sender-first send transfers only when a receiver later
/// arrives):
///   - install the FROZEN snapshot's rights (so a swap to a higher-rights cap at the
///     same index can't escalate — the round-1 fix); and
///   - re-resolve the staged index in `src_proc` and require it STILL names the SAME
///     object (so a close — and, for a reclaimable Channel/Pipe, a close+reuse onto a
///     different resource — is caught; if it's gone or changed, refuse the transfer).
fn transfer_into(src_proc: usize, dst_proc: usize, staging_tid: usize) -> SysResult {
    let n = unsafe { (*slot(staging_tid)).staging.handle_count as usize };
    for i in 0..n {
        let snap = unsafe { (*slot(staging_tid)).staged_entries[i] }.ok_or(SysError::BadHandle)?;
        let src_h = unsafe { (*slot(staging_tid)).staging.handles[i] };
        // Sender must still hold the same object at that index (catches close / swap /
        // reclaim-reuse). We INSTALL the snapshot, not the re-resolved entry, so rights
        // stay frozen.
        let same = proc::with_proc_mut(src_proc, |p| p.get(src_h))
            .map(|e| e.obj == snap.obj)
            .unwrap_or(false);
        if !same {
            return Err(SysError::BadHandle);
        }
        let new_h = proc::with_proc_mut(dst_proc, |p| p.alloc_slot(snap))?;
        unsafe { (*slot(staging_tid)).staging.handles[i] = new_h };
    }
    Ok(())
}

/// Stage `m` into a thread's `staging` slot copying ONLY the used words (header +
/// data_len payload + handle_count handles + badge), not all 552 bytes of MsgBuf. The
/// receiver's `copy_msg_to_user` only ever reads `data[..data_len]`/`handles[..count]`,
/// so the unused tail is never observed — leaving it stale is safe, and skips a ~512 B
/// memcpy on every send (most messages are tiny: an empty call stages ~24 B). §perf.
#[inline]
unsafe fn stage_msg(dst: *mut MsgBuf, m: &MsgBuf) {
    (*dst).tag = m.tag;
    (*dst).data_len = m.data_len;
    (*dst).handle_count = m.handle_count;
    (*dst).badge = m.badge;
    let dl = (m.data_len as usize).min(MSG_DATA_WORDS);
    for i in 0..dl {
        (*dst).data[i] = m.data[i];
    }
    let hc = (m.handle_count as usize).min(MSG_HANDLES);
    for i in 0..hc {
        (*dst).handles[i] = m.handles[i];
    }
}

/// `sys_send`/`sys_call`: `msg` is the dispatcher's validated copy-in (read under
/// our CR3). Returns the final `(rax, rdx)`.
pub fn send_or_call(ep_idx: u8, msg: &MsgBuf, msg_uptr: u64, is_call: bool) -> (u64, u64) {
    let me = thread::current();

    // Freeze our granted handle entries up front, under no lock, in our own
    // non-preemptible syscall window — so the later transfer (which may run after we
    // block, in a receiver's syscall) installs exactly these, immune to a sibling
    // thread mutating our handle table in between (the TOCTOU this closes).
    if let Err(e) = stage_entries(me, thread::current_proc(), msg) {
        return (e as u64, HANDLE_NULL as u64);
    }

    let mut eps = ENDPOINTS.lock();
    let ep = &mut eps[ep_idx as usize];
    if !ep.in_use {
        return (SysError::Gone as u64, HANDLE_NULL as u64);
    }

    if let Some(r) = ep.recv_waiter.take() {
        // ---- receiver-first: a receiver is blocked waiting for us ----
        drop(eps);
        // Deposit our message + the frozen entry snapshot into the receiver's staging,
        // then move the granted handles into the receiver's table.
        unsafe {
            stage_msg(&mut (*slot(r)).staging, msg);
            (*slot(r)).staged_entries = (*slot(me)).staged_entries;
        }
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
        if is_call {
            unsafe { (*slot(me)).msg_uptr = msg_uptr };
            // §70: commit to Blocked BEFORE waking the receiver — it may run on
            // another CPU and `sys_reply` (→ wake us) the instant it wakes. Setting
            // Blocked+barrier first means that wake can't be lost.
            thread::prepare_block();
        }
        thread::wake(r);

        if is_call {
            thread::block_current(); // sleep only if still Blocked; wait for sys_reply
            resume_epilogue(me)
        } else {
            (0, HANDLE_NULL as u64) // one-way send delivered
        }
    } else {
        // ---- sender-first: no receiver yet, stage ourselves and block ----
        unsafe {
            stage_msg(&mut (*slot(me)).staging, msg);
            (*slot(me)).msg_uptr = msg_uptr;
            (*slot(me)).is_call = is_call;
            (*slot(me)).copy_out = false;
        }
        ep.send_q.push(me);
        thread::prepare_block(); // §70: set Blocked under the ENDPOINTS interlock,
                                 // before a receiver can take us off send_q and wake
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
        thread::prepare_block(); // §70: set Blocked under the ENDPOINTS interlock,
                                 // before a sender can deposit + wake us
        drop(eps);
        thread::block_current(); // woken by a sender depositing into our staging
        return resume_epilogue(me);
    }
}

/// Clear `tid` as `ep_idx`'s receiver (used by `recv_notif` to back out of a block
/// it didn't complete via the endpoint).
fn clear_recv_waiter(ep_idx: u8, tid: usize) {
    let mut eps = ENDPOINTS.lock();
    if eps[ep_idx as usize].recv_waiter == Some(tid) {
        eps[ep_idx as usize].recv_waiter = None;
    }
}

/// Multiplexed wait (§sys_recv_notif): block until EITHER a message arrives on
/// `ep_idx` OR `notif_idx` is signalled. Returns `(0, reply_cap)` for a message (like
/// `recv`) or `(0, RECV_NOTIF_FIRED)` for a notif wake. Race-safety: we register as the
/// endpoint receiver and `prepare_block` FIRST (covers the sender, §70), then arm the
/// notif (covers the signal — `arm_bound` re-checks the latched count). A notif signal
/// wakes us WITHOUT depositing, so a concurrent sender handoff (which DOES deposit) is
/// never corrupted; if the sender wins, the notif count stays latched for the next call.
pub fn recv_notif(ep_idx: u8, notif_idx: u8, msg_uptr: u64, timeout_ticks: u64) -> (u64, u64) {
    let me = thread::current();

    loop {
        let mut eps = ENDPOINTS.lock();
        let ep = &mut eps[ep_idx as usize];
        if !ep.in_use {
            return (SysError::Gone as u64, HANDLE_NULL as u64);
        }

        if let Some(s) = ep.send_q.pop() {
            // ---- sender-first: identical to `recv` ----
            let is_call = unsafe { (*slot(s)).is_call };
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
                    Ok((h, _idx)) => h,
                    Err(e) => {
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
            let staged = unsafe { (*slot(s)).staging };
            return match copy_msg_to_user(msg_uptr, &staged) {
                Ok(()) => (0, reply_rdx as u64),
                Err(e) => (e as u64, HANDLE_NULL as u64),
            };
        }

        // ---- no sender: register on the endpoint, then arm the notif ----
        if ep.recv_waiter.is_some() {
            drop(eps);
            return (SysError::NoMem as u64, HANDLE_NULL as u64);
        }
        ep.recv_waiter = Some(me);
        unsafe { (*slot(me)).msg_uptr = msg_uptr };
        thread::prepare_block(); // Blocked under the ENDPOINTS interlock (covers senders)
        drop(eps);

        if !notif::arm_bound(notif_idx, me) {
            // The notif was already latched — don't sleep. Undo the endpoint block.
            thread::cancel_block();
            clear_recv_waiter(ep_idx, me);
            let _ = notif::drain(notif_idx);
            return (0, oxbow_abi::RECV_NOTIF_FIRED);
        }

        // Optional timeout: the timer IRQ (wake_expired) wakes us at the deadline with a
        // plain CAS (no deposit), so a server can pump periodically even if a device IRQ
        // is shared/throttled and its notif doesn't fire. Treated like a notif wake.
        if timeout_ticks > 0 {
            thread::set_wake_at(me, crate::arch::ticks() + timeout_ticks);
        }
        thread::block_current(); // woken by a sender (deposits), a notif signal, or the timer
        if timeout_ticks > 0 {
            thread::set_wake_at(me, 0); // disarm on every exit
        }

        if unsafe { (*slot(me)).copy_out } {
            // A sender delivered a message — drop our notif arming, return the message.
            notif::clear_bound(notif_idx, me);
            return resume_epilogue(me);
        }
        // Notif fired, timed out, or a spurious wake: drop BOTH registrations + drain,
        // so no stale waiter survives this call (else a later signal could wake an
        // unrelated `recv`/`recv_notif` on this thread).
        clear_recv_waiter(ep_idx, me);
        notif::clear_bound(notif_idx, me);
        let _ = notif::drain(notif_idx);
        return (0, oxbow_abi::RECV_NOTIF_FIRED);
    }
}

/// `sys_reply`: deliver `reply` to the caller behind Reply pool slot `reply_idx`,
/// consuming the Reply. Never blocks.
pub fn do_reply(reply_idx: usize, reply: &MsgBuf) {
    let caller = reply_caller(reply_idx);
    unsafe { (*slot(caller)).staging = *reply };
    // Transfer any handles in the reply from the replier's table into the caller's
    // (the reply path mirrors §3.4 — a server returns a freshly-minted cap in OPEN
    // this way). Snapshot the replier's entries into the caller's slot first, then
    // install them (same freeze-then-transfer discipline as the send path).
    let transfer = if reply.handle_count > 0 {
        stage_entries(caller, thread::current_proc(), reply)
            .and_then(|()| transfer_into(thread::current_proc(), thread::process_of(caller), caller))
    } else {
        Ok(())
    };
    unsafe {
        match transfer {
            Ok(()) => {
                (*slot(caller)).copy_out = true;
                (*slot(caller)).ret_rax = 0;
            }
            Err(e) => {
                (*slot(caller)).copy_out = false; // caller couldn't hold the caps
                (*slot(caller)).ret_rax = e as u64;
            }
        }
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
