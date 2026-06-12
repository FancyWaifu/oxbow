//! Process construction.
//!
//! v0 hand-builds the single user process P1 from the Limine module — there is
//! no spawn syscall (ABI §4.2). This phase loads the image and stack and returns
//! the entry point; the capability/handle table arrives in Phase 7.
use oxbow_abi::{
    Handle, SysError, BOOT_CONSOLE, BOOT_EP, HANDLE_TABLE_SIZE, R_ATTENUATE, R_SEND, R_WRITE,
};
use spin::Mutex;

use crate::elf::{perm_str, Image};
use crate::ipc;
use crate::mm::{self, pmm};
use crate::object::{HandleEntry, ObjType, ObjectRef};
use crate::println;

const FRAME: u64 = pmm::FRAME_SIZE;

/// The single v0 process and its flat handle table (ABI §3). One process, one
/// thread, no scheduler — so a plain global behind a spinlock is enough.
pub struct Process {
    handles: [Option<HandleEntry>; HANDLE_TABLE_SIZE],
}

/// The (only) process in v0. Populated at boot by [`load`].
pub static PROCESS: Mutex<Process> = Mutex::new(Process::new());

impl Process {
    pub const fn new() -> Self {
        Process {
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
    /// Order matters and matches the ABI: handle → type → rights.
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

    /// Free a handle slot. Object teardown/refcounting lands in Phase 8.
    pub fn close(&mut self, h: Handle) -> Result<(), SysError> {
        let idx = h as usize;
        if idx == 0 || idx >= HANDLE_TABLE_SIZE || self.handles[idx].is_none() {
            return Err(SysError::BadHandle);
        }
        self.handles[idx] = None;
        Ok(())
    }

    /// Close every handle (on `sys_exit`).
    pub fn close_all(&mut self) {
        self.handles = [None; HANDLE_TABLE_SIZE];
    }
}

/// Top of the user stack (exclusive). Canonical lower-half.
const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;
/// 64 KiB stack.
const USER_STACK_PAGES: u64 = 16;

const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Build P1 from a validated ELF image: map each PT_LOAD W^X-clean (U=1) and a
/// 64 KiB stack with an unmapped guard page below it. Returns `(entry, rsp)`.
pub fn load(img: &Image) -> (u64, u64) {
    let bytes = img.bytes();
    let mut segments = 0u32;

    for ph in img.loads() {
        let writable = ph.p_flags & PF_W != 0;
        let executable = ph.p_flags & PF_X != 0;
        // Enforce W^X at load time (law L4); the mapper asserts again per page.
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
            // Fresh zeroed frame (so the bss tail of the segment is already 0).
            let frame = pmm::alloc_frame().expect("elf: out of frames");

            // Copy the file-backed bytes overlapping this page (if any) through
            // the HHDM RW alias — the user mapping below is RX/R, never W+X.
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

            mm::vm::map_user_4k(page, frame, writable, executable);
            page += FRAME;
        }
        segments += 1;
    }

    // Stack: 64 KiB RW+NX, with the page below left unmapped as a guard.
    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * FRAME;
    for i in 0..USER_STACK_PAGES {
        let frame = pmm::alloc_frame().expect("elf: out of stack frames");
        mm::vm::map_user_4k(stack_base + i * FRAME, frame, true, false);
    }

    // Grant P1 its boot capabilities — the ONLY handles it is born holding (no
    // ambient authority, laws L1/L3). EP0 with send (not recv — the kernel keeps
    // the receive side) and not grant; Console with write. Both attenuable.
    {
        let mut p = PROCESS.lock();
        p.install(
            BOOT_EP,
            HandleEntry {
                obj: ObjectRef::Endpoint(ipc::EP0),
                rights: R_SEND | R_ATTENUATE,
            },
        );
        p.install(
            BOOT_CONSOLE,
            HandleEntry {
                obj: ObjectRef::Console,
                rights: R_WRITE | R_ATTENUATE,
            },
        );
    }

    println!(
        "[proc] P1: {} segment(s), stack {} KiB @ {:#x} (guard below)",
        segments,
        USER_STACK_PAGES * FRAME / 1024,
        stack_base
    );
    println!(
        "[cap] P1 slot {}=Endpoint0(send|attenuate) slot {}=Console(write|attenuate)",
        BOOT_EP, BOOT_CONSOLE
    );

    (img.entry, USER_STACK_TOP)
}
