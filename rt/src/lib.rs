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
    SYS_SPAWN, TAG_TTY_WRITE,
};

// --- Heap (so `alloc` works) ----------------------------------------------
// A bump allocator that lazily grows by `sys_map`ing pages from the program's
// Memory budget (BOOT_MEM) on demand — programs that never allocate pay nothing.
// `dealloc` is a no-op: spawned programs are short-lived, and the whole address
// space (heap included) is reclaimed on exit, so a bump heap is exactly right.
mod heap {
    use core::alloc::{GlobalAlloc, Layout};
    use core::sync::atomic::{AtomicUsize, Ordering};

    const HEAP_BASE: usize = 0x3000_0000;
    const HEAP_LIMIT: usize = 0x3040_0000; // 4 MiB ceiling

    pub struct Bump {
        next: AtomicUsize,       // 0 until first use, then the bump pointer
        mapped_end: AtomicUsize, // highest vaddr currently mapped
    }

    #[global_allocator]
    static HEAP: Bump = Bump {
        next: AtomicUsize::new(0),
        mapped_end: AtomicUsize::new(0),
    };

    unsafe impl GlobalAlloc for Bump {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // Single-threaded program: Relaxed ordering is sufficient.
            let mut next = self.next.load(Ordering::Relaxed);
            if next == 0 {
                next = HEAP_BASE;
                self.mapped_end.store(HEAP_BASE, Ordering::Relaxed);
            }
            let align = layout.align().max(1);
            let start = (next + align - 1) & !(align - 1);
            let end = match start.checked_add(layout.size()) {
                Some(e) if e <= HEAP_LIMIT => e,
                _ => return core::ptr::null_mut(),
            };
            let mut mend = self.mapped_end.load(Ordering::Relaxed);
            if end > mend {
                let need = (end - mend + 0xfff) & !0xfff; // round up to whole pages
                let r = crate::sys_map(
                    super::BOOT_MEM,
                    mend as u64,
                    need as u64,
                    super::PROT_READ | super::PROT_WRITE,
                );
                if r.is_err() {
                    return core::ptr::null_mut();
                }
                mend += need;
                self.mapped_end.store(mend, Ordering::Relaxed);
            }
            self.next.store(end, Ordering::Relaxed);
            start as *mut u8
        }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
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

/// Mint a fresh Endpoint (R_SEND|R_RECV|R_GRANT|R_ATTENUATE) — for a parent to
/// set up an IPC channel between the children it spawns.
pub fn sys_ep_create() -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(SYS_EP_CREATE, 0) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// This program's argument string (the kernel mapped it at SPAWN_ARGV on spawn).
/// Empty if spawned without an argument.
pub fn argv() -> &'static [u8] {
    let p = oxbow_abi::SPAWN_ARGV as *const u8;
    let mut n = 0usize;
    while n < 55 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(p, n) }
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
