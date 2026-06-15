//! oxbow-rt — the userland runtime for oxbow servers.
//!
//! Provides `_start` (the ELF entry), typed `syscall` stubs for the whole v0
//! ABI, and a userland panic handler. A server crate links this, defines
//! `oxbow_main() -> !`, and gets a working ring-3 runtime. See docs/abi-v0.md.
//!
//! It also provides the userland conveniences that make oxbow programs feel like
//! ordinary code: a heap (so `alloc` — `Vec`, `String`, `format!` — works), and
//! `print!`/`println!` to the program's stdout. See `heap` + `io` below + §17.
#![no_std]

extern crate alloc;

use core::panic::PanicInfo;

// Re-exported so servers can `use oxbow_rt::abi` for the shared ABI types.
pub use oxbow_abi as abi;

use oxbow_abi::{
    Handle, MsgBuf, SysError, SysResult, BOOT_CONSOLE, BOOT_MEM, PROT_READ, PROT_WRITE,
    SPAWN_STDOUT, SYS_ATTENUATE, SYS_CALL, SYS_CLOSE, SYS_CONSOLE_WRITE, SYS_EXIT, SYS_FRAME_ALLOC,
    SYS_FRAME_MAP, SYS_IO_IN, SYS_IO_OUT, SYS_IRQ_ACK, SYS_IRQ_BIND, SYS_MAP, SYS_NOTIF_CREATE,
    SYS_NOTIF_SIGNAL, SYS_NOTIF_WAIT, SYS_RECV, SYS_REPLY, SYS_SEND, SYS_EP_CREATE, SYS_MINT,
    SYS_SPAWN, SYS_SPAWN_BYTES, TAG_TTY_WRITE,
};

// --- Heap (so `alloc` works) ----------------------------------------------
// A segregated free-list (slab) allocator. Each request rounds up to a
// power-of-two size class; a per-class free list recycles deallocations, so a
// long-lived server that churns same-size allocations (e.g. the net server
// opening/closing TCP connections, each smoltcp socket needing 4 KiB buffers)
// reuses freed blocks instead of growing without bound. Fresh blocks are carved
// from a bump region that lazily `sys_map`s pages from the Memory budget
// (BOOT_MEM) — programs that never allocate still pay nothing.
//
// Within a class, every block is class-sized and class-aligned, so a freed block
// satisfies any later request mapped to that class. No coalescing across classes:
// same-size reuse is exactly what the workload needs, and the heap is bounded by
// the process budget regardless. Single-threaded (one thread per server), so the
// load/store pairs need no CAS — atomics are only here to satisfy `Sync`.
mod heap {
    use core::alloc::{GlobalAlloc, Layout};
    use core::sync::atomic::{AtomicUsize, Ordering};

    const HEAP_BASE: usize = 0x3000_0000;
    const HEAP_LIMIT: usize = 0x3400_0000; // 64 MiB ceiling (real cap is the budget)
    const MIN_BUCKET: u32 = 4; // smallest class = 16 bytes (holds the free-list link)
    const NBUCKETS: usize = 40; // up to 2^39; the free-list link lives in the block

    /// Power-of-two size class index for a layout: `ceil_log2(max(size, align))`,
    /// floored at MIN_BUCKET. `class = 1 << bucket >= size` and `>= align`.
    fn bucket_of(layout: Layout) -> usize {
        let need = layout.size().max(layout.align()).max(1);
        let b = usize::BITS - (need - 1).leading_zeros();
        b.max(MIN_BUCKET) as usize
    }

    pub struct Slab {
        bump: AtomicUsize,       // next un-carved address (0 until first use)
        mapped_end: AtomicUsize, // highest vaddr currently mapped
        free: [AtomicUsize; NBUCKETS], // per-class free-list heads (0 = empty)
    }

    #[global_allocator]
    static HEAP: Slab = Slab {
        bump: AtomicUsize::new(0),
        mapped_end: AtomicUsize::new(0),
        free: [const { AtomicUsize::new(0) }; NBUCKETS],
    };

    unsafe impl GlobalAlloc for Slab {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let bucket = bucket_of(layout);
            let class = 1usize << bucket;

            // 1. Reuse a freed block of this class (pop the intrusive free list).
            let head = self.free[bucket].load(Ordering::Relaxed);
            if head != 0 {
                let next = *(head as *const usize);
                self.free[bucket].store(next, Ordering::Relaxed);
                return head as *mut u8;
            }

            // 2. Carve a fresh class-sized, class-aligned block from the bump.
            let mut next = self.bump.load(Ordering::Relaxed);
            if next == 0 {
                next = HEAP_BASE;
                self.mapped_end.store(HEAP_BASE, Ordering::Relaxed);
            }
            let start = (next + class - 1) & !(class - 1);
            let end = match start.checked_add(class) {
                Some(e) if e <= HEAP_LIMIT => e,
                _ => return core::ptr::null_mut(),
            };
            let mut mend = self.mapped_end.load(Ordering::Relaxed);
            if end > mend {
                let need = (end - mend + 0xfff) & !0xfff; // whole pages
                if crate::sys_map(
                    super::BOOT_MEM,
                    mend as u64,
                    need as u64,
                    super::PROT_READ | super::PROT_WRITE,
                )
                .is_err()
                {
                    return core::ptr::null_mut();
                }
                mend += need;
                self.mapped_end.store(mend, Ordering::Relaxed);
            }
            self.bump.store(end, Ordering::Relaxed);
            start as *mut u8
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // Push onto this class's free list: stash the old head in the block.
            let bucket = bucket_of(layout);
            let head = self.free[bucket].load(Ordering::Relaxed);
            *(ptr as *mut usize) = head;
            self.free[bucket].store(ptr as usize, Ordering::Relaxed);
        }
    }
}

// --- stdout: print!/println! ----------------------------------------------
// A program's stdout is a tty endpoint at SPAWN_STDOUT (granted by its spawner).
// `Stdout` implements `core::fmt::Write` over it, so `format_args!` and the
// `print!`/`println!` macros below Just Work — no manual MsgBuf packing.

/// Write raw bytes to stdout (the granted tty endpoint), chunked into the tty's
/// <=63-byte TAG_TTY_WRITE messages.
pub fn stdout_write(bytes: &[u8]) {
    let mut off = 0;
    while off < bytes.len() {
        let n = core::cmp::min(63, bytes.len() - off);
        let mut m = MsgBuf::new(TAG_TTY_WRITE);
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(bytes[off..].as_ptr(), dst, n);
            *dst.add(n) = 0;
        }
        m.data_len = ((n + 1 + 7) / 8) as u32;
        let _ = sys_send(SPAWN_STDOUT, &m);
        off += n;
    }
}

/// The stdout sink for `core::fmt`.
pub struct Stdout;
impl core::fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        stdout_write(s.as_bytes());
        Ok(())
    }
}

/// Backs `print!`/`println!`.
pub fn _print(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let _ = Stdout.write_fmt(args);
}

/// Print to stdout (no newline).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::_print(format_args!($($arg)*)));
}

/// Print to stdout with a trailing newline.
#[macro_export]
macro_rules! println {
    () => ($crate::_print(format_args!("\n")));
    ($($arg:tt)*) => ($crate::_print(format_args!("{}\n", format_args!($($arg)*))));
}

// --- File API -------------------------------------------------------------
/// A small client for the fs protocol (§15), the ergonomic half of the "libc":
/// open a path relative to a directory capability, read a whole file into a
/// `Vec`, or iterate a directory.
pub mod fs {
    use crate::{sys_call, Handle};
    use alloc::vec::Vec;
    use oxbow_abi::{MsgBuf, TAG_FS_OPEN, TAG_FS_READ, TAG_FS_READDIR};

    /// A node returned by `open`: its capability, kind (`FS_FILE`/`FS_DIR`), size.
    pub struct Node {
        pub cap: Handle,
        pub kind: u64,
        pub size: usize,
    }

    fn pack(m: &mut MsgBuf, path: &[u8]) {
        let n = core::cmp::min(path.len(), 56);
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(path.as_ptr(), dst, n);
            *dst.add(n) = 0;
        }
        m.data_len = ((n + 1 + 7) / 8) as u32;
    }

    /// Open `path` (may be multi-component) relative to directory cap `dir`.
    pub fn open(dir: Handle, path: &[u8]) -> Option<Node> {
        let mut m = MsgBuf::new(TAG_FS_OPEN);
        pack(&mut m, path);
        if sys_call(dir, &mut m).is_err() || m.data[0] != 0 {
            return None;
        }
        Some(Node {
            cap: m.handles[0],
            kind: m.data[1],
            size: m.data[2] as usize,
        })
    }

    /// Create-or-truncate `path` relative to `dir`; returns the file capability.
    pub fn create(dir: Handle, path: &[u8]) -> Option<Handle> {
        let mut m = MsgBuf::new(oxbow_abi::TAG_FS_CREATE);
        pack(&mut m, path);
        if sys_call(dir, &mut m).is_err() || m.data[0] != 0 {
            return None;
        }
        Some(m.handles[0])
    }

    /// Write all of `bytes` to a file capability, looping in <=48-byte chunks.
    pub fn write_all(file: Handle, bytes: &[u8]) {
        let mut off = 0u64;
        let mut i = 0;
        while i < bytes.len() {
            let n = core::cmp::min(48, bytes.len() - i);
            let mut m = MsgBuf::new(oxbow_abi::TAG_FS_WRITE);
            m.data[0] = off;
            m.data[1] = n as u64;
            let dst = m.data.as_mut_ptr() as *mut u8;
            unsafe { core::ptr::copy_nonoverlapping(bytes[i..].as_ptr(), dst.add(16), n) };
            m.data_len = 8;
            if sys_call(file, &mut m).is_err() {
                break;
            }
            let wrote = m.data[0] as usize;
            if wrote == 0 {
                break;
            }
            off += wrote as u64;
            i += wrote;
        }
    }

    /// Read an entire file capability into a `Vec`.
    pub fn read_all(file: Handle) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0u64;
        loop {
            let mut m = MsgBuf::new(TAG_FS_READ);
            m.data[0] = off;
            m.data_len = 1;
            if sys_call(file, &mut m).is_err() {
                break;
            }
            let count = m.data[0] as usize;
            if count == 0 {
                break;
            }
            let bytes =
                unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), count) };
            out.extend_from_slice(bytes);
            off += count as u64;
        }
        out
    }

    /// Read the directory entry at `cursor` (name, kind), or `None` at the end.
    pub fn readdir(dir: Handle, cursor: u64) -> Option<(Vec<u8>, u64)> {
        let mut m = MsgBuf::new(TAG_FS_READDIR);
        m.data[0] = cursor;
        m.data_len = 1;
        if sys_call(dir, &mut m).is_err() || m.data[0] == 0 {
            return None;
        }
        let kind = m.data[1];
        let bytes =
            unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(16), 48) };
        let n = bytes.iter().position(|&b| b == 0).unwrap_or(0);
        Some((bytes[..n].to_vec(), kind))
    }
}

// --- The server provides this; _start calls it ---------------------------
extern "C" {
    fn oxbow_main() -> !;
}

/// ELF entry point. The kernel enters here at CPL 3 with `rsp` at the top of the
/// stack page. Align the stack, call into the server, and (defensively) exit if
/// it ever returns.
#[unsafe(naked)]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "and rsp, -16",   // 16-byte align (call then makes it %16==8, per SysV)
        "call {main}",
        "ud2",            // oxbow_main is -> !, so this is unreachable
        main = sym start_rust,
    );
}

extern "C" fn start_rust() -> ! {
    unsafe { oxbow_main() }
}

// --- Raw syscall stubs ----------------------------------------------------
// nr in rax; args rdi, rsi, rdx, r10, r8, r9; returns rax (+ rdx). rcx/r11 are
// clobbered by the `syscall` instruction. No `nomem`/`nostack` options: the
// kernel reads/writes user memory (MsgBuf) through these, so the compiler must
// treat them as full memory barriers.

#[inline]
unsafe fn syscall1(nr: u64, a1: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        lateout("rdx") rdx,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

#[inline]
unsafe fn syscall2(nr: u64, a1: u64, a2: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        lateout("rdx") rdx,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

#[inline]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        inlateout("rdx") a3 => rdx,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

#[inline]
unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        inlateout("rdx") a3 => rdx,
        in("r10") a4, // 4th arg per the kernel's SysV-with-r10 convention
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

unsafe fn syscall5(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> (u64, u64) {
    let (rax, rdx);
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => rax,
        in("rdi") a1,
        in("rsi") a2,
        inlateout("rdx") a3 => rdx,
        in("r10") a4,
        in("r8") a5,
        lateout("rcx") _,
        lateout("r11") _,
    );
    (rax, rdx)
}

// --- Typed ABI ------------------------------------------------------------

pub fn sys_send(ep: Handle, msg: *const MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_SEND, ep as u64, msg as u64) };
    SysError::from_raw(rax)
}

pub fn sys_recv(ep: Handle, msg: *mut MsgBuf) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall2(SYS_RECV, ep as u64, msg as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_call(ep: Handle, msg: *mut MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_CALL, ep as u64, msg as u64) };
    SysError::from_raw(rax)
}

pub fn sys_reply(reply: Handle, msg: *const MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_REPLY, reply as u64, msg as u64) };
    SysError::from_raw(rax)
}

pub fn sys_attenuate(src: Handle, new_rights: u32) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall2(SYS_ATTENUATE, src as u64, new_rights as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Mint a BADGED capability to the endpoint `src` (§14): the kernel delivers
/// `badge` to whoever receives a message sent through the returned handle.
/// `src` must be unbadged + held with R_ATTENUATE; `new_rights` ⊆ src; badge != 0.
pub fn sys_mint(src: Handle, badge: u64, new_rights: u32) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall3(SYS_MINT, src as u64, badge, new_rights as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_close(h: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(SYS_CLOSE, h as u64) };
    SysError::from_raw(rax)
}

pub fn sys_console_write(con: Handle, buf: *const u8, len: usize) -> SysResult {
    let (rax, _) = unsafe { syscall3(SYS_CONSOLE_WRITE, con as u64, buf as u64, len as u64) };
    SysError::from_raw(rax)
}

/// Raw syscall escape hatch returning `(rax, rdx)` — for the selftest harness to
/// invoke arbitrary/unknown syscall numbers. Normal code uses the typed wrappers.
pub fn sys_raw(nr: u64, a1: u64, a2: u64, a3: u64) -> (u64, u64) {
    unsafe { syscall3(nr, a1, a2, a3) }
}

pub fn sys_map(mem: Handle, vaddr: u64, len: u64, prot: u64) -> SysResult {
    let (rax, _) = unsafe { syscall4(SYS_MAP, mem as u64, vaddr, len, prot) };
    SysError::from_raw(rax)
}

pub fn sys_frame_alloc(mem: Handle) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_FRAME_ALLOC, mem as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

pub fn sys_frame_map(frame: Handle, vaddr: u64, prot: u64) -> SysResult {
    let (rax, _) = unsafe { syscall3(SYS_FRAME_MAP, frame as u64, vaddr, prot) };
    SysError::from_raw(rax)
}

pub fn sys_notif_create() -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_NOTIF_CREATE, 0) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Spawn a program `image` into a new process. `mem` (a Memory budget) pays;
/// `msg` carries the child budget (`data[0]`) and the capabilities to grant it
/// (`handles`, per the §13 slot convention); `exit_notif` is signalled when the
/// child exits (or HANDLE_NULL for fire-and-forget). Returns the child pid.
pub fn sys_spawn(
    image: Handle,
    mem: Handle,
    msg: *const MsgBuf,
    exit_notif: Handle,
) -> SysResult<u64> {
    let (rax, rdx) =
        unsafe { syscall4(SYS_SPAWN, image as u64, mem as u64, msg as u64, exit_notif as u64) };
    SysError::from_raw(rax).map(|_| rdx)
}

/// exec-from-fs (§33): spawn a new process from an ELF image supplied as bytes
/// (`elf`), rather than from a boot-granted Image cap. Same `mem`/`msg`/
/// `exit_notif` protocol as [`sys_spawn`]. The kernel reads the bytes from the
/// caller's address space and validates the ELF header. Returns the child pid.
pub fn sys_spawn_bytes(
    elf: &[u8],
    mem: Handle,
    msg: *const MsgBuf,
    exit_notif: Handle,
) -> SysResult<u64> {
    let (rax, rdx) = unsafe {
        syscall5(
            SYS_SPAWN_BYTES,
            elf.as_ptr() as u64,
            elf.len() as u64,
            mem as u64,
            msg as u64,
            exit_notif as u64,
        )
    };
    SysError::from_raw(rax).map(|_| rdx)
}

/// Mint a fresh Endpoint (R_SEND|R_RECV|R_GRANT|R_ATTENUATE) — for a parent to
/// set up an IPC channel between the children it spawns.
pub fn sys_ep_create() -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_EP_CREATE, 0) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Read a PCI config-space register of the device `pcidev` (§18).
pub fn sys_pci_read(pcidev: Handle, offset: u32) -> SysResult<u32> {
    let (rax, rdx) =
        unsafe { syscall2(oxbow_abi::SYS_PCI_READ, pcidev as u64, offset as u64) };
    SysError::from_raw(rax).map(|_| rdx as u32)
}

/// Write a PCI config-space register of `pcidev`.
pub fn sys_pci_write(pcidev: Handle, offset: u32, value: u32) -> SysResult {
    let (rax, _) = unsafe {
        syscall3(oxbow_abi::SYS_PCI_WRITE, pcidev as u64, offset as u64, value as u64)
    };
    SysError::from_raw(rax)
}

/// Map the device's memory BAR `bar` (uncacheable) into this AS at `vaddr`.
pub fn sys_pci_bar_map(pcidev: Handle, bar: u32, vaddr: u64) -> SysResult {
    let (rax, _) = unsafe {
        syscall3(oxbow_abi::SYS_PCI_BAR_MAP, pcidev as u64, bar as u64, vaddr)
    };
    SysError::from_raw(rax)
}

/// Framebuffer geometry behind cap `fb`: `(width, height, pitch, bpp)`.
pub fn sys_fb_info(fb: Handle) -> SysResult<(u32, u32, u32, u16)> {
    let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_FB_INFO, fb as u64) };
    SysError::from_raw(rax).map(|_| {
        (
            (rdx & 0xffff) as u32,
            ((rdx >> 16) & 0xffff) as u32,
            ((rdx >> 32) & 0xffff) as u32,
            ((rdx >> 48) & 0xffff) as u16,
        )
    })
}

/// Map the linear framebuffer (RW, uncacheable) into this AS at `vaddr`.
pub fn sys_fb_map(fb: Handle, vaddr: u64) -> SysResult {
    let (rax, _) = unsafe { syscall2(oxbow_abi::SYS_FB_MAP, fb as u64, vaddr) };
    SysError::from_raw(rax)
}

/// Allocate one DMA frame from the `mem` budget, map it writable at `vaddr`, and
/// return its physical address (§19) — for a driver's ring/buffer pointers.
pub fn sys_dma_alloc(mem: Handle, vaddr: u64) -> SysResult<u64> {
    let (rax, rdx) = unsafe { syscall2(oxbow_abi::SYS_DMA_ALLOC, mem as u64, vaddr) };
    SysError::from_raw(rax).map(|_| rdx)
}

/// Change the protection of already-mapped pages (§24 / JIT). `prot` may be
/// PROT_READ|PROT_WRITE or PROT_READ|PROT_EXEC — W^X forbids both at once.
pub fn sys_protect(mem: Handle, vaddr: u64, len: u64, prot: u64) -> SysResult {
    let (rax, _) =
        unsafe { syscall4(oxbow_abi::SYS_PROTECT, mem as u64, vaddr, len, prot) };
    SysError::from_raw(rax)
}

/// Monotonic uptime in milliseconds — an ambient clock for timer-driven code
/// (smoltcp's TCP timers). Not a capability; every process may read the clock.
pub fn sys_uptime_ms() -> u64 {
    let (_, rdx) = unsafe { syscall1(oxbow_abi::SYS_UPTIME_MS, 0) };
    rdx
}

/// This program's argument string (the kernel mapped it at SPAWN_ARGV on spawn).
/// Empty if spawned without an argument.
pub fn argv() -> &'static [u8] {
    let p = oxbow_abi::SPAWN_ARGV as *const u8;
    let mut n = 0usize;
    // The kernel maps a full page (4 KiB) of arguments, NUL-terminated (§13).
    while n < 4095 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(p, n) }
}

/// The program's arguments as whitespace-separated tokens — a real argv vector
/// (`for arg in rt::args()`), built by splitting `argv()`.
pub fn args() -> impl Iterator<Item = &'static [u8]> {
    argv().split(|&b| b == b' ').filter(|s| !s.is_empty())
}

pub fn sys_notif_signal(notif: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(SYS_NOTIF_SIGNAL, notif as u64) };
    SysError::from_raw(rax)
}

/// Block until the notification is signalled; returns the latched signal count.
pub fn sys_notif_wait(notif: Handle) -> SysResult<u64> {
    let (rax, rdx) = unsafe { syscall1(SYS_NOTIF_WAIT, notif as u64) };
    SysError::from_raw(rax).map(|_| rdx)
}

pub fn sys_io_in(ioport: Handle, port: u16) -> SysResult<u8> {
    let (rax, rdx) = unsafe { syscall2(SYS_IO_IN, ioport as u64, port as u64) };
    SysError::from_raw(rax).map(|_| rdx as u8)
}

pub fn sys_io_out(ioport: Handle, port: u16, value: u8) -> SysResult {
    let (rax, _) = unsafe { syscall3(SYS_IO_OUT, ioport as u64, port as u64, value as u64) };
    SysError::from_raw(rax)
}

pub fn sys_irq_bind(irq: Handle, notif: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_IRQ_BIND, irq as u64, notif as u64) };
    SysError::from_raw(rax)
}

pub fn sys_irq_ack(irq: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(SYS_IRQ_ACK, irq as u64) };
    SysError::from_raw(rax)
}

/// Fill `buf` (<=256 bytes) with CSPRNG bytes from the kernel. Returns Err on a
/// too-large or unmapped buffer.
pub fn sys_getentropy(buf: &mut [u8]) -> SysResult {
    let (rax, _) =
        unsafe { syscall2(oxbow_abi::SYS_GETENTROPY, buf.as_mut_ptr() as u64, buf.len() as u64) };
    SysError::from_raw(rax)
}

/// Restrict this process to the given pledge promise classes (PLEDGE_* bits,
/// intersected with the current set). After this, a syscall outside the permitted
/// classes terminates the process. Always succeeds.
pub fn sys_pledge(promises: u64) -> SysResult {
    let (rax, _) = unsafe { syscall1(oxbow_abi::SYS_PLEDGE, promises) };
    SysError::from_raw(rax)
}

/// Permanently lock the protection of a mapped range (mimmutable). After this,
/// sys_map/sys_protect touching the range is refused. Needs the Memory cap.
pub fn sys_immutable(mem: Handle, vaddr: u64, len: u64) -> SysResult {
    let (rax, _) =
        unsafe { syscall3(oxbow_abi::SYS_IMMUTABLE, mem as u64, vaddr, len) };
    SysError::from_raw(rax)
}

/// Create a kernel byte pipe; returns a full-rights handle (attenuate to a read
/// end R_IN and a write end R_OUT). The primitive behind shell pipelines.
pub fn sys_pipe() -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_PIPE, 0) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Read up to `buf.len()` bytes from a pipe (blocks while empty; 0 = EOF). R_IN.
pub fn sys_pipe_read(pipe: Handle, buf: &mut [u8]) -> usize {
    let (rax, rdx) = unsafe {
        syscall3(oxbow_abi::SYS_PIPE_READ, pipe as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
    };
    if SysError::from_raw(rax).is_ok() {
        rdx as usize
    } else {
        0
    }
}

/// Write all of `buf` to a pipe (blocks while full). Returns bytes written. R_OUT.
pub fn sys_pipe_write(pipe: Handle, buf: &[u8]) -> usize {
    let (rax, rdx) = unsafe {
        syscall3(oxbow_abi::SYS_PIPE_WRITE, pipe as u64, buf.as_ptr() as u64, buf.len() as u64)
    };
    if SysError::from_raw(rax).is_ok() {
        rdx as usize
    } else {
        0
    }
}

/// Mark a pipe's write side closed; readers then drain and get EOF. R_OUT.
pub fn sys_pipe_eof(pipe: Handle) -> SysResult {
    let (rax, _) = unsafe { syscall1(oxbow_abi::SYS_PIPE_EOF, pipe as u64) };
    SysError::from_raw(rax)
}

pub fn sys_exit(code: u64) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    // sys_exit never returns; the kernel halts the process. Loop as a backstop.
    loop {
        core::hint::spin_loop();
    }
}

// --- Panic handler --------------------------------------------------------

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Best-effort: announce on the console capability, then exit non-zero.
    let msg = b"pong: panic\n";
    let _ = sys_console_write(BOOT_CONSOLE, msg.as_ptr(), msg.len());
    sys_exit(101)
}

/// UDP socket client over the net server's capability API (§21). `bind` on the
/// NET_CTL control cap returns a fresh badged socket cap; `sendto`/`recvfrom`
/// ride that cap (badge = socket id) so the server stays near-stateless.
pub mod udp {
    use crate::{sys_call, sys_frame_map, Handle};
    use oxbow_abi::{
        MsgBuf, PROT_READ, PROT_WRITE, TAG_NET_DNS, TAG_UDP_ATTACH, TAG_UDP_BIND, TAG_UDP_CLOSE,
        TAG_UDP_RECVFROM, TAG_UDP_RECVV, TAG_UDP_SENDTO, TAG_UDP_SENDV,
    };

    /// Client-side vaddr of the shared UDP transfer frame (large datagram path).
    pub const UDP_XFER: u64 = 0x3E00_0000;

    /// Attach to the net server's shared UDP transfer frame (TAG_UDP_ATTACH on
    /// `ctl`). The server owns the frame and returns a cap to it; we map that same
    /// physical page at `UDP_XFER`. Returns the buffer pointer on success;
    /// thereafter `sendv` sends FROM it and `recvv` receives INTO it, so a whole
    /// (<=1472-byte) UDP datagram moves in one IPC. Call once per process.
    pub fn attach(ctl: Handle) -> Option<*mut u8> {
        let mut m = MsgBuf::new(TAG_UDP_ATTACH);
        if sys_call(ctl, &mut m).is_err() || m.data[0] != 0 || m.handle_count == 0 {
            return None;
        }
        let frame = m.handles[0];
        if sys_frame_map(frame, UDP_XFER, PROT_READ | PROT_WRITE).is_err() {
            return None;
        }
        Some(UDP_XFER as *mut u8)
    }

    /// Send the first `len` bytes of the shared frame to `ip:dport` on `sock`.
    /// Requires a prior `attach`.
    pub fn sendv(sock: Handle, ip: [u8; 4], dport: u16, len: usize) -> bool {
        let mut m = MsgBuf::new(TAG_UDP_SENDV);
        m.data[0] = u32::from_be_bytes(ip) as u64;
        m.data[1] = dport as u64;
        m.data[2] = len.min(1472) as u64;
        m.data_len = 3;
        sys_call(sock, &mut m).is_ok() && m.data[0] == 0
    }

    /// Non-blocking: receive the next datagram for `sock` INTO the shared frame.
    /// Returns its length (0 = nothing buffered now). Requires a prior `attach`.
    pub fn recvv(sock: Handle) -> usize {
        let mut m = MsgBuf::new(TAG_UDP_RECVV);
        if sys_call(sock, &mut m).is_err() {
            return 0;
        }
        (m.data[0] as usize).min(1472)
    }

    /// Close a UDP socket: free the net server's socket slot AND the client cap.
    /// Always use this (not a bare sys_close) — the net slot table is small and a
    /// bind without a matching close leaks a slot.
    pub fn close(sock: Handle) {
        let mut m = MsgBuf::new(TAG_UDP_CLOSE);
        let _ = sys_call(sock, &mut m);
        let _ = crate::sys_close(sock);
    }

    /// The DHCP-leased DNS resolver IP, from the net control cap `ctl`. Falls back
    /// to the SLIRP default if the query fails. Use this instead of a hardcoded
    /// server so resolution works on a real LAN.
    pub fn dns_server(ctl: Handle) -> [u8; 4] {
        let mut m = MsgBuf::new(TAG_NET_DNS);
        if sys_call(ctl, &mut m).is_ok() {
            [m.data[0] as u8, m.data[1] as u8, m.data[2] as u8, m.data[3] as u8]
        } else {
            [10, 0, 2, 3]
        }
    }

    /// Bind a UDP socket via the control cap `ctl`; returns `(socket cap, port)`.
    /// `port` 0 asks the server for an ephemeral port.
    pub fn bind(ctl: Handle, port: u16) -> Option<(Handle, u16)> {
        let mut m = MsgBuf::new(TAG_UDP_BIND);
        m.data[0] = port as u64;
        m.data_len = 1;
        if sys_call(ctl, &mut m).is_err() || m.data[0] != 0 {
            return None;
        }
        Some((m.handles[0], m.data[1] as u16))
    }

    /// Send `payload` (<=40 bytes inline) to `ip:dport` on socket cap `sock`.
    pub fn sendto(sock: Handle, ip: [u8; 4], dport: u16, payload: &[u8]) -> bool {
        let n = payload.len().min(40);
        let mut m = MsgBuf::new(TAG_UDP_SENDTO);
        m.data[0] = u32::from_be_bytes(ip) as u64;
        m.data[1] = dport as u64;
        m.data[2] = n as u64;
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(payload.as_ptr(), dst.add(24), n) };
        m.data_len = 8;
        sys_call(sock, &mut m).is_ok() && m.data[0] == 0
    }

    /// Receive a datagram on `sock` into `out` (blocks); returns payload length.
    pub fn recvfrom(sock: Handle, out: &mut [u8]) -> usize {
        let mut m = MsgBuf::new(TAG_UDP_RECVFROM);
        if sys_call(sock, &mut m).is_err() {
            return 0;
        }
        let n = (m.data[0] as usize).min(out.len()).min(56);
        let src = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), n) };
        out[..n].copy_from_slice(src);
        n
    }
}

/// Minimal DNS A-record client: build a recursive query, parse the first answer.
pub mod dns {
    use alloc::vec::Vec;

    /// Build a standard recursive A-record query for `name` (e.g. "example.com").
    pub fn query(id: u16, name: &str) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&id.to_be_bytes());
        q.extend_from_slice(&0x0100u16.to_be_bytes()); // recursion desired
        q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        q.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
        q.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        q.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
        q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
        q
    }

    fn skip_name(p: &[u8], mut off: usize) -> Option<usize> {
        loop {
            let b = *p.get(off)?;
            if b & 0xC0 == 0xC0 {
                return Some(off + 2);
            }
            if b == 0 {
                return Some(off + 1);
            }
            off += 1 + b as usize;
        }
    }

    /// Parse the first A (IPv4) answer out of a DNS response.
    pub fn first_a(resp: &[u8]) -> Option<[u8; 4]> {
        if resp.len() < 12 {
            return None;
        }
        let qd = u16::from_be_bytes([resp[4], resp[5]]);
        let an = u16::from_be_bytes([resp[6], resp[7]]);
        let mut off = 12;
        for _ in 0..qd {
            off = skip_name(resp, off)?;
            off += 4;
        }
        for _ in 0..an {
            off = skip_name(resp, off)?;
            if off + 10 > resp.len() {
                return None;
            }
            let typ = u16::from_be_bytes([resp[off], resp[off + 1]]);
            let rdlen = u16::from_be_bytes([resp[off + 8], resp[off + 9]]) as usize;
            off += 10;
            if typ == 1 && rdlen == 4 && off + 4 <= resp.len() {
                return Some([resp[off], resp[off + 1], resp[off + 2], resp[off + 3]]);
            }
            off += rdlen;
        }
        None
    }
}

/// TCP socket client over the net server's capability API (§23). `connect` on
/// the NET_CTL control cap returns a fresh badged TCP-socket cap; `send`/`recv`/
/// `close` ride that cap (badge = socket id), same shape as UDP.
pub mod tcp {
    use crate::{sys_call, sys_close, Handle};
    use oxbow_abi::{MsgBuf, TAG_TCP_CLOSE, TAG_TCP_CONNECT, TAG_TCP_RECV, TAG_TCP_SEND};

    /// Open a TCP connection to `ip:port` via control cap `ctl`; returns a socket
    /// cap once the handshake completes (None on refusal/timeout).
    pub fn connect(ctl: Handle, ip: [u8; 4], port: u16) -> Option<Handle> {
        let mut m = MsgBuf::new(TAG_TCP_CONNECT);
        m.data[0] = u32::from_be_bytes(ip) as u64;
        m.data[1] = port as u64;
        m.data_len = 2;
        if sys_call(ctl, &mut m).is_err() || m.data[0] != 0 {
            return None;
        }
        Some(m.handles[0])
    }

    /// Send up to 48 bytes on a TCP socket cap. Returns false on error.
    pub fn send(sock: Handle, data: &[u8]) -> bool {
        let n = data.len().min(48);
        let mut m = MsgBuf::new(TAG_TCP_SEND);
        m.data[0] = n as u64;
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst.add(8), n) };
        m.data_len = 8;
        sys_call(sock, &mut m).is_ok() && m.data[0] == 0
    }

    /// Receive on a TCP socket cap into `out` (blocks server-side until data or
    /// close); returns the byte count (0 = connection closed).
    pub fn recv(sock: Handle, out: &mut [u8]) -> usize {
        let mut m = MsgBuf::new(TAG_TCP_RECV);
        // Tell the server how many bytes we can take, so it consumes only that
        // much from the TCP stream — a smaller read must NOT drop the rest (TLS
        // reads 5-byte record headers; dropped bytes corrupt the stream).
        m.data[0] = out.len().min(56) as u64;
        m.data_len = 1;
        if sys_call(sock, &mut m).is_err() {
            return 0;
        }
        let n = (m.data[0] as usize).min(out.len()).min(56);
        let src = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), n) };
        out[..n].copy_from_slice(src);
        n
    }

    /// Close a TCP socket cap and release the client handle.
    pub fn close(sock: Handle) {
        let mut m = MsgBuf::new(TAG_TCP_CLOSE);
        let _ = sys_call(sock, &mut m);
        let _ = sys_close(sock);
    }
}
