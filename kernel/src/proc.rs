//! Processes — an address space (PML4) + a flat capability handle table.
//!
//! v1 arc 2: a fixed pool of processes (no heap, law L6). Each process owns its
//! own PML4 (per-process isolation, CR3 switched by the scheduler). The "current
//! process" is resolved from the current thread (`thread::current_proc`).
use oxbow_abi::{
    Handle, SysError, BOOT_CONSOLE, BOOT_MEM, HANDLE_TABLE_SIZE, R_ATTENUATE, R_GRANT, R_MAP,
    R_OUT, R_WRITE, SPAWN_ARGV, SPAWN_IDENT,
};
use crate::sync::DiagMutex;

use crate::elf::{perm_str, Image};
use crate::ipc;
use crate::mm::{self, pmm};
use crate::object::{HandleEntry, ObjType, ObjectRef};
use crate::println;

const FRAME: u64 = pmm::FRAME_SIZE;

/// Maximum concurrent processes (static pool). Must keep pace with
/// `thread::MAX_THREADS` — every spawned program is a process, so the desktop
/// (oxcomp + 3 clients) + servers + user commands need the same headroom, or `ls`
/// runs out of process slots just like it ran out of TCB slots.
pub const MAX_PROCS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PState {
    Free,
    Alive,
    Dead,
}

/// PT_TLS template for §101 native ELF thread-local storage. Each thread gets its
/// own TLS block copied from this template; `vaddr` is where `.tdata` lives in the
/// process address space (so spawned threads can re-read it live).
#[derive(Clone, Copy)]
pub struct TlsTemplate {
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

/// A process: its address space root and its capability handle table (ABI §3).
#[derive(Clone, Copy)]
pub struct Process {
    pub state: PState,
    pub pml4_phys: u64,
    /// PT_TLS template (None if the image has no thread-local storage).
    pub tls: Option<TlsTemplate>,
    /// Bump allocator for per-thread TLS block vaddrs (one frame each).
    pub tls_next: u64,
    handles: [Option<HandleEntry>; HANDLE_TABLE_SIZE],
    /// Notification the kernel signals when this process exits (set at spawn).
    exit_notif: Option<u8>,
    /// This process's own Memory budget pool index, released on exit.
    mem_idx: Option<u8>,
    /// The spawner's Memory budget index + the cost it paid — refunded on exit.
    parent_mem: Option<u8>,
    spawn_cost: u64,
    /// Permitted syscall-class bitmask (pledge, §37). u64::MAX = unpledged (all
    /// classes). `sys_pledge` only ever intersects it, so authority is monotone.
    pledge: u64,
    /// Immutable address ranges (mimmutable, §38): [start, end) pairs whose
    /// protection can never change again. Fixed pool; `imm_count` are live.
    imm: [(u64, u64); MAX_IMM],
    imm_count: usize,
    /// Process name (NUL-padded), set at spawn — reported by SYS_PROC_LIST (ps).
    name: [u8; 16],
    /// §Phase 9 step 2: the userland async-signal dispatcher address this process
    /// registered (SYS_SIGDISPATCH), or 0. When set, an async signal injects a frame
    /// redirecting here instead of terminating the process.
    pub sig_dispatch: u64,
}

/// Max immutable ranges per process (a runtime locks text + maybe rodata/got).
const MAX_IMM: usize = 8;

static PROCESSES: DiagMutex<[Process; MAX_PROCS]> = DiagMutex::new("PROCESSES", [Process::new(); MAX_PROCS]);

impl Process {
    pub const fn new() -> Self {
        Process {
            state: PState::Free,
            pml4_phys: 0,
            tls: None,
            tls_next: TLS_REGION_BASE,
            handles: [None; HANDLE_TABLE_SIZE],
            exit_notif: None,
            mem_idx: None,
            parent_mem: None,
            spawn_cost: 0,
            pledge: u64::MAX, // unpledged: every syscall class permitted
            imm: [(0, 0); MAX_IMM],
            imm_count: 0,
            name: [0; 16],
            sig_dispatch: 0,
        }
    }

    /// True if this process may invoke a syscall of the given class (all bits of
    /// `class` must be permitted). Class 0 (exit/pledge/close) is always allowed.
    pub fn pledge_allows(&self, class: u64) -> bool {
        self.pledge & class == class
    }

    /// Narrow the pledge to the intersection with `mask` (drop authority only).
    pub fn pledge_narrow(&mut self, mask: u64) {
        self.pledge &= mask;
    }

    /// Mark [start, end) immutable. Returns false if the range table is full.
    pub fn immutable_add(&mut self, start: u64, end: u64) -> bool {
        if self.imm_count >= MAX_IMM {
            return false;
        }
        self.imm[self.imm_count] = (start, end);
        self.imm_count += 1;
        true
    }

    /// True if [start, end) overlaps any immutable range (so a map/protect over
    /// it must be refused).
    pub fn is_immutable(&self, start: u64, end: u64) -> bool {
        self.imm[..self.imm_count].iter().any(|&(s, e)| start < e && s < end)
    }

    /// Install a well-known handle at a fixed slot (boot-time setup only).
    pub fn install(&mut self, slot: Handle, entry: HandleEntry) {
        self.handles[slot as usize] = Some(entry);
    }

    /// Fetch an entry by handle with no type/rights check (for `attenuate`).
    pub fn get(&self, h: Handle) -> Result<HandleEntry, SysError> {
        let idx = h as usize;
        if idx == 0 || idx >= HANDLE_TABLE_SIZE {
            return Err(SysError::BadHandle);
        }
        self.handles[idx].ok_or(SysError::BadHandle)
    }

    /// Look up a handle, enforcing the expected object type and rights (law L2).
    /// Order matches the ABI: handle → type → rights.
    pub fn lookup(&self, h: Handle, ty: ObjType, rights: u32) -> Result<HandleEntry, SysError> {
        let entry = self.get(h)?;
        if entry.obj.ty() != ty {
            return Err(SysError::BadType);
        }
        if entry.rights & rights != rights {
            return Err(SysError::Rights);
        }
        Ok(entry)
    }

    /// Place an entry in the lowest free slot (≥ 1); `E_NO_SLOTS` if full.
    pub fn alloc_slot(&mut self, entry: HandleEntry) -> Result<Handle, SysError> {
        for i in 1..HANDLE_TABLE_SIZE {
            if self.handles[i].is_none() {
                self.handles[i] = Some(entry);
                return Ok(i as Handle);
            }
        }
        Err(SysError::NoSlots)
    }

    /// Free a handle slot.
    pub fn close(&mut self, h: Handle) -> Result<(), SysError> {
        let idx = h as usize;
        if idx == 0 || idx >= HANDLE_TABLE_SIZE || self.handles[idx].is_none() {
            return Err(SysError::BadHandle);
        }
        self.handles[idx] = None;
        Ok(())
    }

    /// Close every handle.
    pub fn close_all(&mut self) {
        self.handles = [None; HANDLE_TABLE_SIZE];
    }

    /// Call `f` with the pool index of every Reply handle this process holds.
    pub fn for_each_reply(&self, mut f: impl FnMut(u8)) {
        for h in self.handles.iter().flatten() {
            if let ObjectRef::Reply(idx) = h.obj {
                f(idx);
            }
        }
    }
}

/// §Phase 11 pipe writer-refcount: does any ALIVE process still hold a write end
/// (an R_OUT Pipe handle) of pipe `pidx`? Used to decide EOF when a write end drops.
fn any_pipe_writer(procs: &[Process; MAX_PROCS], pidx: u8) -> bool {
    procs.iter().any(|p| {
        matches!(p.state, PState::Alive)
            && p.handles.iter().flatten().any(|h| {
                matches!(h.obj, ObjectRef::Pipe(i) if i == pidx) && (h.rights & R_OUT) != 0
            })
    })
}

/// Atomically CLAIM a Reply handle: if `h` names a live Reply in the current process,
/// remove it from the table and return its pool index — all under one PROCESSES lock.
/// This is the single point that consumes a reply, so two cores invoking sys_reply (or
/// sys_reply racing SYS_CLOSE) can't both resolve-then-free the same pool slot (a
/// double-free + double-wake). The loser sees the slot already gone (BadHandle).
pub fn take_reply(h: Handle) -> Result<u8, SysError> {
    let mut procs = PROCESSES.lock();
    let id = crate::thread::current_proc();
    let idx = h as usize;
    if idx == 0 || idx >= HANDLE_TABLE_SIZE {
        return Err(SysError::BadHandle);
    }
    let entry = procs[id].handles[idx].ok_or(SysError::BadHandle)?;
    let ObjectRef::Reply(ridx) = entry.obj else {
        return Err(SysError::BadType);
    };
    // Reject a STALE reply handle: if the slot was freed (caller died / already replied)
    // and reused for a different caller since this handle was minted, its generation no
    // longer matches — delivering through it would misdirect the reply to the wrong
    // client. Drop the dead handle and refuse.
    if !crate::ipc::reply_gen_ok(ridx as usize, entry.badge as u32) {
        procs[id].handles[idx] = None;
        return Err(SysError::BadHandle);
    }
    procs[id].handles[idx] = None; // claim it — a concurrent reply/close now misses
    Ok(ridx)
}

/// A side effect the SYS_CLOSE dispatcher must perform AFTER `close_handle` returns
/// (i.e. after the PROCESSES lock is dropped) — both involve waking threads, which must
/// not happen under that lock (the v0 lock rule).
pub enum CloseAction {
    Nothing,
    /// The last pipe write end closed — mark pipe `pidx` EOF and wake its readers.
    PipeEof(u8),
    /// A live Reply handle was closed (a server dropping a call) — abandon reply slot
    /// `ridx` so its pool slot is freed and the blocked caller wakes with E_GONE.
    AbandonReply(usize),
}

/// Close the current process's handle `h`. Returns a `CloseAction` describing any
/// deferred wake the dispatcher must do once the lock is released. §Phase 11.
pub fn close_handle(h: Handle) -> Result<CloseAction, SysError> {
    let mut procs = PROCESSES.lock();
    let id = crate::thread::current_proc();
    let idx = h as usize;
    if idx == 0 || idx >= HANDLE_TABLE_SIZE || procs[id].handles[idx].is_none() {
        return Err(SysError::BadHandle);
    }
    let entry = procs[id].handles[idx].unwrap();
    procs[id].handles[idx] = None;
    if let ObjectRef::Pipe(pidx) = entry.obj {
        if (entry.rights & R_OUT) != 0 && !any_pipe_writer(&procs, pidx) {
            return Ok(CloseAction::PipeEof(pidx));
        }
    }
    // A Reply handle closed without replying would otherwise LEAK its global pool slot
    // (only 8 exist) and hang the caller forever. Tell the dispatcher to abandon it.
    if let ObjectRef::Reply(ridx) = entry.obj {
        return Ok(CloseAction::AbandonReply(ridx as usize));
    }
    Ok(CloseAction::Nothing)
}

/// Run `f` with the calling thread's process. The lock is statement-scoped and
/// never held across a context switch or an IPC side effect (the v0 lock rule).
pub fn with_current<R>(f: impl FnOnce(&Process) -> R) -> R {
    let id = crate::thread::current_proc();
    let procs = PROCESSES.lock();
    f(&procs[id])
}
pub fn with_current_mut<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    let id = crate::thread::current_proc();
    let mut procs = PROCESSES.lock();
    f(&mut procs[id])
}

/// Run `f` with a specific process by id (e.g. an IPC peer's table). The lock is
/// never held across a context switch.
pub fn with_proc_mut<R>(id: usize, f: impl FnOnce(&mut Process) -> R) -> R {
    let mut procs = PROCESSES.lock();
    f(&mut procs[id])
}

/// Record a spawned child's lifecycle: exit notification, its own Memory budget,
/// and the spawner's budget + the cost it paid (refunded when the child dies).
pub fn set_lifecycle(id: usize, exit_notif: Option<u8>, mem_idx: u8, parent_mem: u8, cost: u64) {
    let mut procs = PROCESSES.lock();
    procs[id].exit_notif = exit_notif;
    procs[id].mem_idx = Some(mem_idx);
    procs[id].parent_mem = Some(parent_mem);
    procs[id].spawn_cost = cost;
}

/// §103 Command::kill: the live process whose exit notification is `notif_idx` — i.e.
/// the child a spawner controls via the exit-notif handle it holds. `None` if it has
/// already exited. This is the authority check: holding the child's exit notif (the
/// lifecycle handle the spawner created + passed to spawn) is what permits killing it.
pub fn find_by_exit_notif(notif_idx: u8) -> Option<usize> {
    let procs = PROCESSES.lock();
    (0..MAX_PROCS)
        .find(|&i| matches!(procs[i].state, PState::Alive) && procs[i].exit_notif == Some(notif_idx))
}

/// Snapshot live/dead processes into `out` (for SYS_PROC_LIST / ps). Returns the
/// number filled. Each row is (pid, state: 1=alive 2=dead, 16-byte NUL-padded name).
pub fn snapshot(out: &mut [(u8, u8, [u8; 16])]) -> usize {
    let procs = PROCESSES.lock();
    let mut n = 0;
    for i in 0..MAX_PROCS {
        if n >= out.len() {
            break;
        }
        let st = match procs[i].state {
            PState::Alive => 1u8,
            PState::Dead => 2u8,
            PState::Free => continue,
        };
        out[n] = (i as u8, st, procs[i].name);
        n += 1;
    }
    n
}

/// Kill process `pid` by id (SYS_KILL / the ambient `kill` tool, pledge-gated at the
/// syscall layer). Returns false if `pid` is out of range or not alive.
pub fn kill_pid(pid: usize, code: i32) -> bool {
    if pid >= MAX_PROCS {
        return false;
    }
    if !matches!(PROCESSES.lock()[pid].state, PState::Alive) {
        return false;
    }
    kill(pid, code);
    true
}

/// The async-signal dispatcher a live process registered (0 = none / not alive).
/// §Phase 9 step 2: used to decide inject-vs-terminate on async Ctrl-C.
pub fn sig_dispatch_of(pid: usize) -> u64 {
    if pid >= MAX_PROCS {
        return 0;
    }
    let procs = PROCESSES.lock();
    if matches!(procs[pid].state, PState::Alive) {
        procs[pid].sig_dispatch
    } else {
        0
    }
}

/// Mark a process dead and drop its handles (on a ring-3 fault or `sys_exit`).
/// Any pending Reply it held is abandoned — the blocked caller wakes `E_GONE`.
/// Releases the child's Memory slot, REFUNDS the spawner's budget by the cost it
/// paid, and signals the exit notification. The address space is NOT freed here
/// (it may be the live CR3 — the dying thread is still running on it); its frames
/// are reclaimed when the slot is reused (`create`). The slot becomes `Dead`.
pub fn kill(id: usize, code: i32) {
    // §Phase 11: pipes whose last write end the dying process held must EOF (its
    // readers — e.g. a shell capturing $(...) — would otherwise block forever).
    let mut eof_pipes = [0u8; 16];
    let mut eof_n = 0usize;
    let (exit_notif, mem_idx, parent_mem, cost) = {
        let mut procs = PROCESSES.lock();
        // Gather the pipe write ends this process holds (deduped).
        let mut wp = [0u8; 16];
        let mut wn = 0usize;
        for h in procs[id].handles.iter().flatten() {
            if let ObjectRef::Pipe(pidx) = h.obj {
                if (h.rights & R_OUT) != 0 && wn < wp.len() && !wp[..wn].contains(&pidx) {
                    wp[wn] = pidx;
                    wn += 1;
                }
            }
        }
        procs[id].for_each_reply(|idx| ipc::reply_abandon(idx as usize));
        procs[id].close_all();
        procs[id].state = PState::Dead; // now this proc is skipped by any_pipe_writer
        for k in 0..wn {
            if !any_pipe_writer(&procs, wp[k]) && eof_n < eof_pipes.len() {
                eof_pipes[eof_n] = wp[k];
                eof_n += 1;
            }
        }
        (
            procs[id].exit_notif.take(),
            procs[id].mem_idx.take(),
            procs[id].parent_mem.take(),
            procs[id].spawn_cost,
        )
    };
    // Outside the PROCESSES lock: EOF the orphaned pipes + wake their blocked readers.
    for k in 0..eof_n {
        let mut wake = [0usize; 8];
        let n = crate::pipe::mark_eof(eof_pipes[k], &mut wake);
        for &t in &wake[..n] {
            crate::thread::wake(t);
        }
    }
    // Outside the PROCESSES lock (lock rule): reclaim the budget slot, refund the
    // spawner, wake the parent.
    if let Some(mi) = mem_idx {
        mm::mem::release(mi);
    }
    if let (Some(pm), c) = (parent_mem, cost) {
        if c > 0 {
            mm::mem::credit(pm, c);
        }
    }
    if let Some(en) = exit_notif {
        // §81: deliver the exit code so a waiting parent (the shell) can branch on
        // it for `&&`/`||`. A clean exit passes its sys_exit code; a fault/pledge
        // death passes a nonzero sentinel.
        crate::notif::signal_exit(en, code);
    }
}

/// Top of the user stack (exclusive). Canonical lower-half.
const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;
/// 512 KiB stack. The DRIFT crypto (curve25519 scalar mult + BLAKE2b) is very
/// stack-hungry in a debug build (it overflowed 64 KiB), so give every process
/// generous headroom — frames come from the pmm, not the process budget.
const USER_STACK_PAGES: u64 = 128;
/// Max ASLR slide (in pages) applied to the stack top — see `load_into`.
const STACK_ASLR_PAGES: u64 = 512;

/// Do `[a0,a1)` and `[b0,b1)` overlap?
fn ranges_overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
    a0 < b1 && b0 < a1
}

/// Validate that an untrusted ELF's PT_LOAD segments don't overlap each other OR the
/// regions the loader maps AFTER them (argv, ident, the ASLR stack window, the TLS
/// region). Without this, a crafted segment placed on a reserved vaddr makes a later
/// `map_to` panic (`PageAlreadyMapped`) → whole-system DoS from any spawn-capable
/// process. Returns false (reject the spawn) on any collision. Assumes
/// `segments_in_bounds` already passed (so vaddr+memsz is lower-half, no overflow).
pub fn elf_layout_ok(img: &Image) -> bool {
    // Reserved windows the loader will map post-segments (page-granular, half-open).
    let stack_lo = USER_STACK_TOP - (STACK_ASLR_PAGES + USER_STACK_PAGES) * FRAME;
    let reserved: [(u64, u64); 4] = [
        (SPAWN_ARGV, SPAWN_ARGV + FRAME),
        (SPAWN_IDENT, SPAWN_IDENT + FRAME),
        (stack_lo, USER_STACK_TOP),
        // Generous TLS guard (per-thread frames bump up from the base).
        (TLS_REGION_BASE, TLS_REGION_BASE + 0x1000_0000),
    ];
    let mut prev: [(u64, u64); 16] = [(0, 0); 16];
    let mut nprev = 0usize;
    for ph in img.loads() {
        let lo = ph.p_vaddr & !(FRAME - 1);
        let hi = (ph.p_vaddr + ph.p_memsz + FRAME - 1) & !(FRAME - 1);
        if hi <= lo {
            continue; // empty segment
        }
        for &(r0, r1) in &reserved {
            if ranges_overlap(lo, hi, r0, r1) {
                return false;
            }
        }
        for &(p0, p1) in &prev[..nprev] {
            if ranges_overlap(lo, hi, p0, p1) {
                return false;
            }
        }
        if nprev < prev.len() {
            prev[nprev] = (lo, hi);
            nprev += 1;
        } else {
            return false; // absurd number of segments — refuse rather than skip a check
        }
    }
    true
}

const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Base of the per-thread TLS region (§101). Far below the stack and above the
/// image; one frame is bump-allocated per thread (`Process::tls_next`).
pub const TLS_REGION_BASE: u64 = 0x0000_7000_0000_0000;

/// Build one per-thread TLS block (x86-64 variant II): a fresh user RW frame mapped
/// at the process's next TLS vaddr, with `.tdata` (`t.filesz` bytes from `src`, a
/// pointer readable in the CURRENT address space) at the bottom, `.tbss` zeroed, and
/// the TCB self-pointer at the thread pointer. Returns the thread pointer (= %fs
/// base). The static-TLS block sits BELOW the TP, so a TLS symbol at template offset
/// `s` is reached at `%fs:(s - tls_size)` = user `vaddr + s` (local-exec model).
fn build_tls_block(
    pml4_phys: u64,
    tls_next: &mut u64,
    t: &TlsTemplate,
    src: *const u8,
) -> Option<u64> {
    let align = t.align.max(8);
    let tls_size = (t.memsz + align - 1) & !(align - 1);
    assert!(tls_size + 16 <= FRAME, "tls: block too large for one frame");
    let vaddr = *tls_next;
    *tls_next += FRAME;
    // OOM is NOT fatal: return None so the caller can fail the spawn/thread-spawn with
    // E_NOMEM (and reclaim any partial address space) instead of panicking the kernel.
    let frame = pmm::alloc_frame()?;
    let kbase = mm::phys_to_virt(frame) as *mut u8;
    let tp = vaddr + tls_size;
    unsafe {
        core::ptr::write_bytes(kbase, 0, FRAME as usize);
        // Defense-in-depth: clamp the copy to the TLS block size so a crafted PT_TLS
        // (filesz > the one-frame block) can NEVER overrun into adjacent kernel frames,
        // even if it somehow slipped past segments_in_bounds. The validator is the gate;
        // this is the wall.
        let copy = core::cmp::min(t.filesz, tls_size) as usize;
        if copy > 0 {
            core::ptr::copy_nonoverlapping(src, kbase, copy);
        }
        // TCB self-pointer at the thread pointer (ABI: *tp == tp).
        *(kbase.add(tls_size as usize) as *mut u64) = tp;
    }
    mm::vm::map_user_4k_in(pml4_phys, vaddr, frame, true, false); // RW, NX
    Some(tp)
}

/// Set up the TLS block for a NEWLY SPAWNED thread of a live process, reading the
/// `.tdata` template from the process's own mapped memory (the current address space
/// is the process's when it calls SYS_THREAD_SPAWN). Returns the thread pointer, or 0
/// if the process has no TLS. §101.
pub fn build_thread_tls(proc_id: usize) -> Option<u64> {
    let mut procs = PROCESSES.lock();
    let p = &mut procs[proc_id];
    // No TLS template is a SUCCESS with fs_base 0; only a frame-alloc failure is None,
    // so the thread-spawn caller can tell "no TLS" apart from "out of memory".
    let Some(t) = p.tls else { return Some(0) };
    let pml4 = p.pml4_phys;
    let mut next = p.tls_next;
    let tp = build_tls_block(pml4, &mut next, &t, t.vaddr as *const u8)?;
    p.tls_next = next;
    Some(tp)
}

/// Map an ELF image's PT_LOAD segments (W^X-clean, U=1) and a guarded 64 KiB
/// What `load_into` produces: the entry/stack plus the main thread's TLS pointer
/// and the (template, next-vaddr) state to copy onto the Process afterwards.
struct LoadResult {
    entry: u64,
    user_rsp: u64,
    fs_base: u64,
    tls: Option<TlsTemplate>,
    tls_next: u64,
}

/// stack into the given address space. The `fs_base` in the result is the main
/// thread's TLS thread pointer (0 if the image has no TLS).
/// Returns `None` on PMM exhaustion partway through (some frames may already be mapped
/// into `pml4_phys`; the caller reclaims the partial address space with `free_user_pml4`
/// and fails the spawn with E_NOMEM — never a kernel panic).
/// §96: the embedded dynamic linker (ld-oxbow) ELF bytes, set at boot from the
/// Limine module. `load_into` maps it alongside any dynamically-linked image.
static INTERP_PTR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static INTERP_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn set_interp(bytes: &'static [u8]) {
    use core::sync::atomic::Ordering;
    INTERP_PTR.store(bytes.as_ptr() as usize, Ordering::Relaxed);
    INTERP_LEN.store(bytes.len(), Ordering::Relaxed);
}
fn interp_image() -> Option<Image<'static>> {
    use core::sync::atomic::Ordering;
    let p = INTERP_PTR.load(Ordering::Relaxed);
    let l = INTERP_LEN.load(Ordering::Relaxed);
    if p == 0 || l == 0 {
        return None;
    }
    let bytes = unsafe { core::slice::from_raw_parts(p as *const u8, l) };
    Image::try_validate(bytes).ok()
}

/// Map an image's PT_LOAD segments into `pml4_phys`; returns the segment count.
/// Shared by the executable and (for a dynamic image) the interpreter.
fn map_loads(img: &Image, pml4_phys: u64) -> Option<u32> {
    let bytes = img.bytes();
    let mut segments = 0u32;
    for ph in img.loads() {
        let writable = ph.p_flags & PF_W != 0;
        let executable = ph.p_flags & PF_X != 0;
        assert!(!(writable && executable), "elf: W|X segment rejected (law L4)");
        if crate::verbose() {
            println!(
                "[elf]   load {:#x} {} filesz={} memsz={}",
                ph.p_vaddr,
                perm_str(ph.p_flags),
                ph.p_filesz,
                ph.p_memsz
            );
        }

        let v_start = ph.p_vaddr;
        let v_end = ph.p_vaddr + ph.p_memsz;
        let file_end = v_start + ph.p_filesz;
        let v_end_aligned = (v_end + FRAME - 1) & !(FRAME - 1);

        let mut page = v_start & !(FRAME - 1);
        while page < v_end_aligned {
            let frame = pmm::alloc_frame()?;
            let copy_start = core::cmp::max(page, v_start);
            let copy_end = core::cmp::min(page + FRAME, file_end);
            if copy_end > copy_start {
                let dst_off = copy_start - page;
                let src_off = (ph.p_offset + (copy_start - v_start)) as usize;
                let len = (copy_end - copy_start) as usize;
                assert!(src_off + len <= bytes.len(), "elf: segment data past module end");
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(src_off),
                        (mm::phys_to_virt(frame) + dst_off) as *mut u8,
                        len,
                    );
                }
            }
            mm::vm::map_user_4k_in(pml4_phys, page, frame, writable, executable);
            page += FRAME;
        }
        segments += 1;
    }
    Some(segments)
}

fn load_into(img: &Image, pml4_phys: u64) -> Option<LoadResult> {
    let segments = map_loads(img, pml4_phys)?;

    // §96: a dynamically-linked image (has PT_INTERP) → also map the interpreter
    // (ld-oxbow) into this address space plus a read-only DynInfo page, and enter
    // the interpreter instead of the executable. ld-oxbow links the shared objects,
    // then jumps to the executable's real entry (DynInfo.exe_entry).
    let mut entry = img.entry;
    if img.interp().is_some() {
        let interp = interp_image()?;
        map_loads(&interp, pml4_phys)?;
        entry = interp.entry;
        let exe_phdr = img.phdr_vaddr()?;
        let info = oxbow_abi::DynInfo {
            magic: oxbow_abi::DYN_INFO_MAGIC,
            exe_entry: img.entry,
            exe_phdr,
            exe_phnum: img.phnum(),
            exe_phent: img.phentsize(),
            exe_base: 0,
        };
        let frame = pmm::alloc_frame()?;
        unsafe {
            let dst = mm::phys_to_virt(frame) as *mut u8;
            core::ptr::write_bytes(dst, 0, FRAME as usize);
            core::ptr::write(dst as *mut oxbow_abi::DynInfo, info);
        }
        mm::vm::map_user_4k_in(pml4_phys, oxbow_abi::DYN_INFO, frame, false, false);
        if crate::verbose() {
            println!("[elf] dynamic: interp entry {:#x}, exe entry {:#x}", entry, img.entry);
        }
    }

    // Stack-base ASLR: slide the stack top down by a random, page-aligned offset
    // (up to ~2 MiB = 9 bits of entropy) so no two processes — and no two boots —
    // share a stack layout. Cheap partial ASLR that needs no PIE; the program's
    // fixed text/argv mappings sit far below and are unaffected.
    let slide = (crate::rng::next_u64() % 512) * FRAME;
    let stack_top = USER_STACK_TOP - slide;
    let stack_base = stack_top - USER_STACK_PAGES * FRAME;
    for i in 0..USER_STACK_PAGES {
        let frame = pmm::alloc_frame()?;
        mm::vm::map_user_4k_in(pml4_phys, stack_base + i * FRAME, frame, true, false);
    }

    if crate::verbose() {
        println!(
            "[elf] {} segment(s), stack {} KiB @ {:#x} (ASLR slide {:#x}, guard below)",
            segments,
            USER_STACK_PAGES * FRAME / 1024,
            stack_base,
            slide
        );
    }

    // §101 native ELF TLS: if the image has a PT_TLS template, set up the MAIN
    // thread's TLS block now, reading `.tdata` straight from the image bytes (the
    // segments are in `pml4_phys`, not the live CR3, so we can't read by vaddr yet).
    // The template + bumped vaddr are returned for the caller to store on the Process.
    let mut fs_base = 0u64;
    let mut tls = None;
    let mut tls_next = TLS_REGION_BASE;
    if let Some(ph) = img.tls() {
        let t = TlsTemplate {
            vaddr: ph.p_vaddr,
            filesz: ph.p_filesz,
            memsz: ph.p_memsz,
            align: ph.p_align,
        };
        let src = unsafe { img.bytes().as_ptr().add(ph.p_offset as usize) };
        fs_base = build_tls_block(pml4_phys, &mut tls_next, &t, src)?;
        tls = Some(t);
        if crate::verbose() {
            println!(
                "[elf] TLS template filesz={} memsz={} align={} -> main tp={:#x}",
                ph.p_filesz, ph.p_memsz, ph.p_align, fs_base
            );
        }
    }
    Some(LoadResult { entry, user_rsp: stack_top, fs_base, tls, tls_next })
}

/// Create a process: claim a pool slot (reusing a `Dead` one), map the image
/// into `pml4_phys`, and return `(proc id, entry, user_rsp)`. Grants NO handles
/// — the caller installs the boot set (`grant_standard` + per-name device caps)
/// or the spawn set (the §13 convention). `E_NOMEM` if the pool is full.
pub fn create(
    img: &Image,
    pml4_phys: u64,
    name: &str,
) -> Result<(usize, u64, u64, u64), SysError> {
    let (id, dead_pml4) = {
        let mut procs = PROCESSES.lock();
        // §103: a Dead slot is reusable only once ALL its threads are gone — reusing
        // it frees the old address space, which a still-winding-down killed thread is
        // running on (use-after-free otherwise). Free slots are always reusable.
        let id = match (0..MAX_PROCS).find(|&i| match procs[i].state {
            PState::Free => true,
            PState::Dead => !crate::thread::proc_has_live_threads(i),
            _ => false,
        }) {
            Some(i) => i,
            // Pool full: the caller's fresh (still-empty) pml4 would leak otherwise —
            // create() owns its cleanup on every Err path.
            None => {
                drop(procs);
                mm::vm::free_user_pml4(pml4_phys);
                return Err(SysError::NoMem);
            }
        };
        // If we're reusing a Dead slot, its old address space is no longer live
        // (the owner switched away on exit) — reclaim its frames below.
        let dead_pml4 = (procs[id].state == PState::Dead).then_some(procs[id].pml4_phys);
        procs[id] = Process::new(); // clear all stale state (handles, lifecycle)
        procs[id].state = PState::Alive;
        procs[id].pml4_phys = pml4_phys;
        let nb = name.as_bytes();
        let n = core::cmp::min(nb.len(), 15);
        procs[id].name[..n].copy_from_slice(&nb[..n]);
        (id, dead_pml4)
    };

    // Free the previous tenant's address space (frames + page tables) now that the
    // PROCESSES lock is dropped and that AS is provably not the live CR3.
    if let Some(old) = dead_pml4 {
        mm::vm::free_user_pml4(old);
    }

    // OOM partway through building the address space: reclaim the partial AS (frames +
    // page tables already mapped), release the just-claimed pool slot, and fail with
    // E_NOMEM — a memory-pressure spawn must never panic the kernel.
    let Some(lr) = load_into(img, pml4_phys) else {
        {
            let mut procs = PROCESSES.lock();
            procs[id] = Process::new(); // back to Free; reusable
        }
        mm::vm::free_user_pml4(pml4_phys);
        return Err(SysError::NoMem);
    };
    // Record the TLS template + bumped vaddr on the Process (lock dropped during the
    // mm-heavy load_into; re-take it for this small write).
    if lr.tls.is_some() {
        let mut procs = PROCESSES.lock();
        procs[id].tls = lr.tls;
        procs[id].tls_next = lr.tls_next;
    }
    if crate::verbose() {
        println!("[proc] {} = proc {} (as pml4={:#x})", name, id, pml4_phys);
    }
    Ok((id, lr.entry, lr.user_rsp, lr.fs_base))
}

/// The address-space root (CR3 value) of process `id` — for a cross-AS copy (a bulk
/// reply writes into the blocked caller's AS without switching CR3).
pub fn pml4_of(id: usize) -> u64 {
    PROCESSES.lock()[id].pml4_phys
}

/// Reclaim a process slot that was `create`d but never started (no thread has run on
/// its address space yet) — frees its address space (frames + page tables) and returns
/// the slot to `Free`. Used when a spawn fails AFTER `create` succeeds (e.g. the budget
/// grant can't be funded), so neither the slot nor the address space leaks. MUST be
/// called before any thread is scheduled on the AS (nothing is executing on it).
pub fn release_unstarted(id: usize) {
    let pml4 = {
        let mut procs = PROCESSES.lock();
        let pml4 = procs[id].pml4_phys;
        procs[id] = Process::new(); // -> Free, reusable
        pml4
    };
    mm::vm::free_user_pml4(pml4);
}

/// Grant a boot process its standard birth capabilities: a Console (write) and a
/// fork (§Phase 3b): clone the calling process's address space + handle table into a
/// NEW process, give it its own Memory budget + the supplied exit notification, and
/// start its main thread at `entry`/`user_rsp` (the personality's trampoline, which
/// `longjmp`s the child to the fork point in its OWN copied AS — same virtual
/// addresses, separate physical, so no shared-stack hazard). Returns the child pid,
/// or 0 on failure. `notif_idx` is the parent's exit-notif for the child (so waitpid
/// works); the TLS is copied with the AS, so the child reuses the parent's fs_base.
pub fn fork_current(entry: u64, user_rsp: u64, notif_idx: Option<u8>) -> u64 {
    let parent = crate::thread::current_proc();
    let fs_base = crate::thread::current_fs_base();
    let parent_pml4 = PROCESSES.lock()[parent].pml4_phys;

    // Heavy work (page copy + budget) OUTSIDE the PROCESSES lock (lock rule).
    let child_pml4 = mm::vm::clone_user_as(parent_pml4);
    if child_pml4 == 0 {
        return 0;
    }
    const FORK_BUDGET: u64 = 32 * 1024 * 1024; // enough for the child to exec
    let Some(child_mem) = mm::mem::grant(FORK_BUDGET) else {
        mm::vm::free_user_pml4(child_pml4);
        return 0;
    };

    let (child, dead_pml4) = {
        let mut procs = PROCESSES.lock();
        let slot = (0..MAX_PROCS).find(|&i| match procs[i].state {
            PState::Free => true,
            PState::Dead => !crate::thread::proc_has_live_threads(i),
            _ => false,
        });
        let Some(id) = slot else {
            drop(procs);
            mm::mem::release(child_mem);
            mm::vm::free_user_pml4(child_pml4);
            return 0;
        };
        let dead = (procs[id].state == PState::Dead).then_some(procs[id].pml4_phys);
        // Copy the parent Process — clones the handle table (fd/cap inheritance),
        // identity, TLS template, pledge, name — then override the per-process fields.
        let mut copy = procs[parent];
        copy.pml4_phys = child_pml4;
        copy.mem_idx = Some(child_mem);
        copy.exit_notif = notif_idx;
        copy.parent_mem = None; // fresh grant; nothing to refund the spawner
        copy.spawn_cost = 0;
        copy.state = PState::Alive;
        // Repoint the cloned BOOT_MEM cap to the child's OWN pool, so its allocations
        // debit its budget and its exit frees its pool (never the parent's).
        copy.install(
            BOOT_MEM,
            HandleEntry {
                obj: ObjectRef::Memory(child_mem),
                rights: R_MAP | R_GRANT | R_ATTENUATE,
                badge: mm::mem::mem_gen(child_mem) as u64, // stale-grant guard
            },
        );
        procs[id] = copy;
        (id, dead)
    };
    if let Some(old) = dead_pml4 {
        mm::vm::free_user_pml4(old);
    }
    crate::thread::spawn_user(child, child_pml4, entry, user_rsp, fs_base);
    child as u64
}

/// fresh Memory budget. Per-name device/endpoint caps are installed separately
/// by the boot loop. (Spawned processes get the §13 set instead — never this.)
pub fn grant_standard(id: usize, budget: u64) {
    // Mint the budget BEFORE taking the PROCESSES lock (lock rule: never hold it
    // across an mm allocation that takes the MEMORY lock).
    let mem_idx = mm::mem::grant(budget).expect("boot: Memory pool exhausted");
    {
        let mut procs = PROCESSES.lock();
        let p = &mut procs[id];
        p.install(
            BOOT_CONSOLE,
            HandleEntry {
                obj: ObjectRef::Console,
                rights: R_WRITE | R_ATTENUATE | R_GRANT,
            badge: 0,
            },
        );
        p.install(
            BOOT_MEM,
            HandleEntry {
                obj: ObjectRef::Memory(mem_idx),
                rights: R_MAP | R_GRANT | R_ATTENUATE,
                badge: mm::mem::mem_gen(mem_idx) as u64, // stale-grant guard
            },
        );
    }
    if crate::verbose() {
        println!("[mem] proc {} granted Memory#{} = {} B (slot {})", id, mem_idx, budget, BOOT_MEM);
    }
}
