//! Processes — an address space (PML4) + a flat capability handle table.
//!
//! v1 arc 2: a fixed pool of processes (no heap, law L6). Each process owns its
//! own PML4 (per-process isolation, CR3 switched by the scheduler). The "current
//! process" is resolved from the current thread (`thread::current_proc`).
use oxbow_abi::{
    Handle, SysError, BOOT_CONSOLE, BOOT_EP, BOOT_MEM, HANDLE_TABLE_SIZE, R_ATTENUATE, R_GRANT,
    R_MAP, R_WRITE,
};
use spin::Mutex;

use crate::elf::{perm_str, Image};
use crate::ipc;
use crate::mm::{self, pmm};
use crate::object::{HandleEntry, ObjType, ObjectRef};
use crate::println;

const FRAME: u64 = pmm::FRAME_SIZE;

/// Maximum concurrent processes (static pool).
pub const MAX_PROCS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PState {
    Free,
    Alive,
    Dead,
}

/// A process: its address space root and its capability handle table (ABI §3).
#[derive(Clone, Copy)]
pub struct Process {
    pub state: PState,
    pub pml4_phys: u64,
    handles: [Option<HandleEntry>; HANDLE_TABLE_SIZE],
}

static PROCESSES: Mutex<[Process; MAX_PROCS]> = Mutex::new([Process::new(); MAX_PROCS]);

impl Process {
    pub const fn new() -> Self {
        Process {
            state: PState::Free,
            pml4_phys: 0,
            handles: [None; HANDLE_TABLE_SIZE],
        }
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

/// Mark a process dead and drop its handles (on a ring-3 fault or `sys_exit`).
/// Any pending Reply it held is abandoned — the blocked caller wakes `E_GONE`.
pub fn kill(id: usize) {
    let mut procs = PROCESSES.lock();
    procs[id].for_each_reply(|idx| ipc::reply_abandon(idx as usize));
    procs[id].close_all();
    procs[id].state = PState::Dead;
}

/// Top of the user stack (exclusive). Canonical lower-half.
const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;
/// 64 KiB stack.
const USER_STACK_PAGES: u64 = 16;

const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Map an ELF image's PT_LOAD segments (W^X-clean, U=1) and a guarded 64 KiB
/// stack into the given address space. Returns `(entry, user_rsp)`.
fn load_into(img: &Image, pml4_phys: u64) -> (u64, u64) {
    let bytes = img.bytes();
    let mut segments = 0u32;

    for ph in img.loads() {
        let writable = ph.p_flags & PF_W != 0;
        let executable = ph.p_flags & PF_X != 0;
        assert!(!(writable && executable), "elf: W|X segment rejected (law L4)");
        println!(
            "[elf]   load {:#x} {} filesz={} memsz={}",
            ph.p_vaddr,
            perm_str(ph.p_flags),
            ph.p_filesz,
            ph.p_memsz
        );

        let v_start = ph.p_vaddr;
        let v_end = ph.p_vaddr + ph.p_memsz;
        let file_end = v_start + ph.p_filesz;
        let v_end_aligned = (v_end + FRAME - 1) & !(FRAME - 1);

        let mut page = v_start & !(FRAME - 1);
        while page < v_end_aligned {
            let frame = pmm::alloc_frame().expect("elf: out of frames");
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

    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * FRAME;
    for i in 0..USER_STACK_PAGES {
        let frame = pmm::alloc_frame().expect("elf: out of stack frames");
        mm::vm::map_user_4k_in(pml4_phys, stack_base + i * FRAME, frame, true, false);
    }

    println!(
        "[elf] {} segment(s), stack {} KiB @ {:#x} (guard below)",
        segments,
        USER_STACK_PAGES * FRAME / 1024,
        stack_base
    );
    (img.entry, USER_STACK_TOP)
}

/// Create a process: claim a pool slot, map the image into `pml4_phys`, grant
/// the boot capabilities, and return `(proc id, entry, user_rsp)`. `ep0_rights`
/// is the role: the pinger gets `R_SEND`, the ponger gets `R_RECV`.
pub fn create(img: &Image, pml4_phys: u64, name: &str, ep0_rights: u32) -> (usize, u64, u64) {
    let id = {
        let mut procs = PROCESSES.lock();
        let id = (0..MAX_PROCS)
            .find(|&i| procs[i].state == PState::Free)
            .expect("proc: out of process slots");
        procs[id].state = PState::Alive;
        procs[id].pml4_phys = pml4_phys;
        id
    };

    let (entry, user_rsp) = load_into(img, pml4_phys);

    // Grant the ONLY handles the process is born holding (laws L1/L3): EP0 with
    // the role-specific rights, Console with write.
    {
        let mut procs = PROCESSES.lock();
        let p = &mut procs[id];
        p.install(
            BOOT_EP,
            HandleEntry {
                obj: ObjectRef::Endpoint(ipc::EP0),
                rights: ep0_rights,
            },
        );
        p.install(
            BOOT_CONSOLE,
            HandleEntry {
                obj: ObjectRef::Console,
                // R_GRANT so a process can attenuate + hand its console to a peer.
                rights: R_WRITE | R_ATTENUATE | R_GRANT,
            },
        );
        // A birth Memory budget — the only authority to allocate (law L6).
        let mem_idx = mm::mem::grant(mm::mem::BOOT_BUDGET);
        p.install(
            BOOT_MEM,
            HandleEntry {
                obj: ObjectRef::Memory(mem_idx),
                rights: R_MAP | R_GRANT | R_ATTENUATE,
            },
        );
        println!(
            "[mem] proc {} granted Memory#{} = {} B (slot {})",
            id, mem_idx, mm::mem::BOOT_BUDGET, BOOT_MEM
        );
    }

    println!("[proc] {} = proc {} (as pml4={:#x})", name, id, pml4_phys);
    (id, entry, user_rsp)
}
