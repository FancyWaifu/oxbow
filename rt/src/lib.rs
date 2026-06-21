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
    SPAWN_STDIN, SPAWN_STDOUT, SYS_ATTENUATE, SYS_CALL, SYS_CLOSE, SYS_CONSOLE_WRITE, SYS_EXIT, SYS_FRAME_ALLOC,
    SYS_FRAME_MAP, SYS_IO_IN, SYS_IO_OUT, SYS_IRQ_ACK, SYS_IRQ_BIND, SYS_MAP, SYS_NOTIF_CREATE,
    SYS_NOTIF_SIGNAL, SYS_NOTIF_STATUS, SYS_NOTIF_WAIT, SYS_RECV, SYS_REPLY, SYS_SEND, SYS_EP_CREATE, SYS_MINT,
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
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // §96: the slab is now reachable from multiple threads of one process (std
    // `thread::spawn`), so its free-list/bump load-store pairs need mutual
    // exclusion. A test-and-set spinlock: critical sections are a handful of
    // instructions, and ring-3 threads are preemptible (IF=1), so a holder that
    // is preempted is always rescheduled to release it — no permanent deadlock.
    static LOCK: AtomicBool = AtomicBool::new(false);

    struct HeapLock;
    impl HeapLock {
        #[inline]
        fn acquire() -> HeapLock {
            while LOCK
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
            HeapLock
        }
    }
    impl Drop for HeapLock {
        #[inline]
        fn drop(&mut self) {
            LOCK.store(false, Ordering::Release);
        }
    }

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

    #[cfg(not(feature = "hosted"))] // §95: std supplies the allocator when hosted
    #[global_allocator]
    static HEAP: Slab = Slab {
        bump: AtomicUsize::new(0),
        mapped_end: AtomicUsize::new(0),
        free: [const { AtomicUsize::new(0) }; NBUCKETS],
    };
    #[cfg(feature = "hosted")]
    static HEAP: Slab = Slab {
        bump: AtomicUsize::new(0),
        mapped_end: AtomicUsize::new(0),
        free: [const { AtomicUsize::new(0) }; NBUCKETS],
    };

    // §95: hosted shims — a Rust `std` program's `sys/pal/oxbow` System allocator
    // calls these instead of using HEAP as the `#[global_allocator]` (std owns
    // that). realloc is left to std's `realloc_fallback`.
    #[cfg(feature = "hosted")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __oxbow_alloc(size: usize, align: usize) -> *mut u8 {
        HEAP.alloc(Layout::from_size_align_unchecked(size, align))
    }
    #[cfg(feature = "hosted")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __oxbow_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
        HEAP.alloc_zeroed(Layout::from_size_align_unchecked(size, align))
    }
    #[cfg(feature = "hosted")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn __oxbow_dealloc(ptr: *mut u8, size: usize, align: usize) {
        HEAP.dealloc(ptr, Layout::from_size_align_unchecked(size, align))
    }

    unsafe impl GlobalAlloc for Slab {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _g = HeapLock::acquire(); // §96: released on every return path
            let bucket = bucket_of(layout);
            // §105: an absurd request (e.g. `try_reserve(isize::MAX)`) lands in a
            // bucket past the table — fail fast. Indexing `free[bucket]` would panic
            // out-of-bounds WHILE holding HeapLock, and the panic's own allocation
            // would then self-deadlock on the spinlock (a hang, not an error).
            if bucket >= NBUCKETS {
                return core::ptr::null_mut();
            }
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
            let _g = HeapLock::acquire(); // §96: serialize free-list pushes
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

/// stdout routing mode, resolved on the first write (§81): 0 = unknown, 1 = a tty
/// endpoint (the normal case — send TAG_TTY_WRITE messages), 2 = a pipe write end
/// (a pipeline stage — use `sys_pipe_write`). A spawner makes stdout a pipe by
/// granting a pipe's R_OUT end at SPAWN_STDOUT instead of a tty endpoint.
static STDOUT_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// §97b The process's current-working-directory capability. Starts at slot 1 — the
/// cwd dir cap the spawner installed. `std::env::set_current_dir` re-roots it
/// (`__oxbow_chdir`) by opening a new dir cap and storing its handle here, so every
/// subsequent *relative* fs op (open/mkdir/unlink/rename) and child spawn resolves
/// against the new directory. The original slot 1 is never closed; re-rooted caps are.
#[cfg(feature = "hosted")]
static CWD_HANDLE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// The handle the relative-path fs shims send to. Defaults to slot 1.
#[cfg(feature = "hosted")]
fn cwd_handle() -> Handle {
    CWD_HANDLE.load(core::sync::atomic::Ordering::Relaxed) as Handle
}

/// Write all of `bytes` into pipe handle `pipe`, looping over partial writes. The
/// kernel blocks the writer while the pipe is full, so this never busy-spins; a
/// return of 0 means the read end closed (broken pipe) — stop.
fn pipe_write_all(pipe: Handle, bytes: &[u8]) {
    let mut off = 0;
    while off < bytes.len() {
        let w = sys_pipe_write(pipe, &bytes[off..]);
        if w == 0 {
            break;
        }
        off += w;
    }
}

/// Write raw bytes to stdout. Normally stdout is a tty endpoint and the bytes go
/// out as <=63-byte TAG_TTY_WRITE messages; but when stdout is a pipe write end
/// (a `cmd | …` stage), `sys_send` reports BadType and we switch to writing the
/// pipe — so a program's print path "just works" whether piped or not.
pub fn stdout_write(bytes: &[u8]) {
    use core::sync::atomic::Ordering;
    if STDOUT_MODE.load(Ordering::Relaxed) == 2 {
        pipe_write_all(SPAWN_STDOUT, bytes);
        return;
    }
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
        match sys_send(SPAWN_STDOUT, &m) {
            Ok(()) => STDOUT_MODE.store(1, Ordering::Relaxed),
            Err(SysError::BadType) if STDOUT_MODE.load(Ordering::Relaxed) == 0 => {
                // stdout is a pipe, not a tty endpoint — write the rest as bytes.
                STDOUT_MODE.store(2, Ordering::Relaxed);
                pipe_write_all(SPAWN_STDOUT, &bytes[off..]);
                return;
            }
            Err(_) => return,
        }
        off += n;
    }
}

/// Read up to `buf.len()` bytes from stdin (the pipe read end a pipeline owner
/// granted at SPAWN_STDIN). Returns the byte count, or 0 at end of input (the
/// write side closed). Blocks while the pipe is empty. A program reads stdin only
/// when it expects to — e.g. `cat -`.
pub fn stdin_read(buf: &mut [u8]) -> usize {
    sys_pipe_read(SPAWN_STDIN, buf)
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
        /// ext2 mtime/atime (Unix epoch seconds, 0 if the server didn't report them).
        pub mtime: u32,
        pub atime: u32,
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
            mtime: if m.data_len >= 4 { m.data[3] as u32 } else { 0 },
            atime: if m.data_len >= 5 { m.data[4] as u32 } else { 0 },
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

// §95: hosted C-ABI shims for a Rust `std` program's `sys/pal/oxbow` (stdio via
// the boot console, randomness, exit). Compiled only with the `hosted` feature.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_write(_fd: i32, buf: *const u8, len: usize) -> isize {
    // §96: use the SAME path as rt::println! — TAG_TTY_WRITE to SPAWN_STDOUT with a
    // pipe fallback (`stdout_write`). A shell-spawned program's stdout is a tty
    // endpoint or a pipe, NOT a kernel Console cap, so the old
    // sys_console_write(BOOT_CONSOLE) silently dropped output (and std's stdout layer
    // then wedged). stdout/stderr both route here.
    let slice = unsafe { core::slice::from_raw_parts(buf, len) };
    stdout_write(slice);
    len as isize
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_read(_fd: i32, _buf: *mut u8, _len: usize) -> isize {
    0 // no console read path yet
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_getentropy(buf: *mut u8, len: usize) -> i32 {
    let slice = core::slice::from_raw_parts_mut(buf, len);
    match sys_getentropy(slice) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_exit(code: i32) -> ! {
    sys_exit(code as u64)
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_uptime_ms() -> u64 {
    sys_uptime_ms()
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_walltime(secs: *mut u64, nanos: *mut u32) {
    let (s, n) = sys_walltime();
    unsafe {
        secs.write(s);
        nanos.write(n as u32);
    }
}

// POSIX personality (musl) shims: set the TLS base (arch_prctl ARCH_SET_FS) and
// anonymous mmap for musl's allocator.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_set_fsbase(addr: u64) {
    let _ = unsafe { syscall1(oxbow_abi::SYS_SET_FSBASE, addr) };
}

/// Anonymous mmap for the POSIX personality (musl's allocator). Bumps a dedicated
/// vaddr window [0x4000_0000, 0x6000_0000) — separate from rt's own heap — and
/// backs it RW with SYS_MAP. No unmap yet (Phase 1); returns (void*)-1 (MAP_FAILED)
/// on exhaustion or map failure.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_mmap_anon(len: usize) -> *mut u8 {
    use core::sync::atomic::{AtomicUsize, Ordering};
    const BASE: usize = 0x4000_0000;
    const LIMIT: usize = 0x6000_0000;
    static CURSOR: AtomicUsize = AtomicUsize::new(BASE);
    let need = (len + 0xfff) & !0xfff;
    if need == 0 {
        return usize::MAX as *mut u8;
    }
    let start = CURSOR.fetch_add(need, Ordering::Relaxed);
    if start + need > LIMIT {
        return usize::MAX as *mut u8;
    }
    if sys_map(BOOT_MEM, start as u64, need as u64, PROT_READ | PROT_WRITE).is_err() {
        return usize::MAX as *mut u8;
    }
    start as *mut u8
}

// §96 hosted shims for std's pal thread + futex backend.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_futex_wait(addr: *const u32, expected: u32, timeout_ms: u64) -> i32 {
    // returns 1 if the wait timed out, 0 otherwise.
    let (timed_out, _) =
        unsafe { syscall3(oxbow_abi::SYS_FUTEX_WAIT, addr as u64, expected as u64, timeout_ms) };
    timed_out as i32
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_futex_wake(addr: *const u32) -> u32 {
    (unsafe { sys_futex_wake(addr, 1) }) as u32
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_futex_wake_all(addr: *const u32) {
    unsafe { sys_futex_wake(addr, u32::MAX) };
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_thread_spawn(
    stack_top: u64,
    entry: extern "C" fn(u64) -> !,
    arg: u64,
) -> u64 {
    let sp = ((stack_top as usize) & !0xF) - 16;
    unsafe {
        (sp as *mut u64).write(entry as u64);
        ((sp + 8) as *mut u64).write(arg);
        sys_thread_spawn(thread_trampoline as u64, sp as u64) as u64
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_thread_exit(done_addr: u64) -> ! {
    unsafe {
        syscall1(oxbow_abi::SYS_THREAD_EXIT, done_addr);
    }
    loop {
        core::hint::spin_loop();
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_thread_id() -> u64 {
    let (tid, _) = unsafe { syscall1(oxbow_abi::SYS_THREAD_ID, 0) };
    tid
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_yield() {
    unsafe {
        syscall1(oxbow_abi::SYS_YIELD, 0);
    }
}
/// §96: the spawn argument string (SPAWN_ARGV) for std's `env::args()`. Writes the
/// length and returns the pointer; the kernel mapped it on spawn (the boot-module
/// cmdline, or the shell's argv).
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_argv(len: *mut usize) -> *const u8 {
    let a = argv();
    unsafe {
        len.write(a.len());
    }
    a.as_ptr()
}

// §97 std::fs shims — open/read/write/close on the fsd protocol, relative to the
// program's cwd dir cap (slot 1). Positioned (offset-based) so std::fs::File works.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_open(
    path: *const u8,
    path_len: usize,
    flags: u32, // FS_O_CREATE | FS_O_EXCL | FS_O_TRUNC
    size_out: *mut u64,
    kind_out: *mut i32, // 1=FS_DIR, 2=FS_FILE, 3=FS_SYMLINK
    mtime_out: *mut u32,
    atime_out: *mut u32,
) -> i64 {
    // One round trip: fsd applies the whole OpenOptions policy (create / exclusive /
    // truncate) itself, so the client never has to stat first. Returns the file cap, or
    // a negative status: -1 NotFound, -2 AlreadyExists (O_EXCL), -3 other error.
    let p = unsafe { core::slice::from_raw_parts(path, path_len) };
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_OPEN);
    let n = core::cmp::min(p.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(p.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data[oxbow_abi::MSG_DATA_WORDS - 1] = flags as u64; // open flags ride in the last word
    m.data_len = oxbow_abi::MSG_DATA_WORDS as u32; // transfer the whole inline area (path + flags)
    if sys_call(cwd_handle(), &mut m).is_err() {
        return -3;
    }
    match m.data[0] {
        0 => {
            unsafe {
                size_out.write(m.data[2]);
                kind_out.write(m.data[1] as i32);
                mtime_out.write(m.data[3] as u32);
                atime_out.write(m.data[4] as u32);
            }
            m.handles[0] as i64
        }
        2 => -2, // AlreadyExists
        3 => -4, // IsADirectory (create/truncate/write on a directory)
        _ => -1, // NotFound / generic
    }
}

/// set_len: truncate the file capability to `size` bytes. 0 ok, -1 on failure.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_truncate(file: i64, size: u64) -> i32 {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_TRUNCATE);
    m.data[0] = size;
    m.data_len = 1;
    if sys_call(file as Handle, &mut m).is_err() || m.data[0] != 0 { -1 } else { 0 }
}

/// Set mtime/atime (Unix epoch seconds) on the file capability; `set_m`/`set_a` gate each.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_set_times(
    file: i64,
    mtime: u32,
    atime: u32,
    set_m: i32,
    set_a: i32,
) -> i32 {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_SETTIMES);
    m.data[0] = mtime as u64;
    m.data[1] = atime as u64;
    m.data[2] = ((set_m != 0) as u64) | (((set_a != 0) as u64) << 1);
    m.data_len = 3;
    if sys_call(file as Handle, &mut m).is_err() || m.data[0] != 0 { -1 } else { 0 }
}

/// Pack two NUL-terminated byte strings `a\0b\0` into a message's inline data area
/// (each capped at fsd's PLEN=200), returning the data_len in words.
#[cfg(feature = "hosted")]
unsafe fn pack_two(m: &mut MsgBuf, a: &[u8], b: &[u8]) {
    let al = a.len().min(200);
    let bl = b.len().min(200);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(a.as_ptr(), dst, al);
        *dst.add(al) = 0;
        core::ptr::copy_nonoverlapping(b.as_ptr(), dst.add(al + 1), bl);
        *dst.add(al + 1 + bl) = 0;
    }
    m.data_len = (((al + 1 + bl + 1) + 7) / 8) as u32;
}

/// Create a symlink at `link` containing the literal `target`. 0 ok, -1 on failure.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_symlink(
    target: *const u8,
    target_len: usize,
    link: *const u8,
    link_len: usize,
) -> i32 {
    let t = unsafe { core::slice::from_raw_parts(target, target_len) };
    let l = unsafe { core::slice::from_raw_parts(link, link_len) };
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_SYMLINK);
    unsafe { pack_two(&mut m, t, l) };
    if sys_call(cwd_handle(), &mut m).is_err() || m.data[0] != 0 { -1 } else { 0 }
}

/// Read the symlink at `path` into `buf` (cap `buf_cap`). Returns the byte count, or -1.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_readlink(
    path: *const u8,
    path_len: usize,
    buf: *mut u8,
    buf_cap: usize,
) -> isize {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_READLINK);
    let n = path_len.min(56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(path, dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    if sys_call(cwd_handle(), &mut m).is_err() {
        return -1;
    }
    let len = m.data[0] as usize;
    if len == 0 {
        return -1; // a real symlink target is non-empty; 0 signals error
    }
    let copy = len.min(buf_cap);
    let src = unsafe { (m.data.as_ptr() as *const u8).add(8) };
    unsafe { core::ptr::copy_nonoverlapping(src, buf, copy) };
    copy as isize
}

/// Create a hard link `dst` referring to the same inode as `src`. 0 ok, -1 on failure.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_link(
    src: *const u8,
    src_len: usize,
    dst: *const u8,
    dst_len: usize,
) -> i32 {
    let s = unsafe { core::slice::from_raw_parts(src, src_len) };
    let d = unsafe { core::slice::from_raw_parts(dst, dst_len) };
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_LINK);
    unsafe { pack_two(&mut m, s, d) };
    if sys_call(cwd_handle(), &mut m).is_err() || m.data[0] != 0 { -1 } else { 0 }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_pread(file: i64, buf: *mut u8, len: usize, off: u64) -> isize {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_READ);
    m.data[0] = off;
    m.data_len = 1;
    if sys_call(file as Handle, &mut m).is_err() {
        return -1;
    }
    let count = (m.data[0] as usize).min(len);
    if count > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping((m.data.as_ptr() as *const u8).add(8), buf, count);
        }
    }
    count as isize
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_pwrite(file: i64, buf: *const u8, len: usize, off: u64) -> isize {
    // Up to 480 payload bytes ride one message (the 512 B inline area past the
    // offset+count header at byte 16), ~10x fewer round trips than the old 48 B cap.
    // std's write_all loops on the returned count, so a short write here is fine.
    let n = len.min(480);
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_WRITE);
    m.data[0] = off;
    m.data[1] = n as u64;
    unsafe {
        core::ptr::copy_nonoverlapping(buf, (m.data.as_mut_ptr() as *mut u8).add(16), n);
    }
    m.data_len = ((16 + n + 7) / 8) as u32; // words covering the header + payload
    if sys_call(file as Handle, &mut m).is_err() {
        return -1;
    }
    m.data[0] as isize
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_fs_close(file: i64) {
    // Tell fsd we're done with this path (drops its intern refcount so the slot is
    // reclaimed), THEN drop the capability. ONE-WAY send, not a call: we don't need fsd's
    // reply, and SEND returns as soon as fsd receives the message (immediately when fsd is
    // idle in recv — the common case) instead of waiting for it to process + reply. Safe:
    // fsd is single-threaded and receives in arrival order, so this RELEASE is always
    // processed before our next OPEN (sent later in program order) — the intern slot is
    // freed before any reusing open. Best-effort: a failed send just defers the reclaim to
    // unlink, as before. fsd replies to the (null) reply cap harmlessly.
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_RELEASE);
    m.data_len = 0;
    let _ = sys_send(file as Handle, &m);
    let _ = sys_close(file as Handle);
}
/// std::fs::create_dir — TAG_FS_MKDIR(name) to the cwd dir cap (slot 1).
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_mkdir(path: *const u8, len: usize) -> i32 {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_MKDIR);
    let n = len.min(56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(path, dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    if sys_call(cwd_handle(), &mut m).is_err() || m.data[0] != 0 {
        -1
    } else {
        0
    }
}
/// std::fs::read_dir entry at `cursor` on an open dir cap. Writes the name into
/// `name_out` (returns its length) + the kind (FS_DIR/FS_FILE); -1 past the end.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_readdir(
    dir: i64,
    cursor: u64,
    name_out: *mut u8,
    name_cap: usize,
    kind_out: *mut u32,
) -> isize {
    match fs::readdir(dir as Handle, cursor) {
        Some((name, kind)) => {
            let n = name.len().min(name_cap);
            unsafe {
                core::ptr::copy_nonoverlapping(name.as_ptr(), name_out, n);
                kind_out.write(kind as u32);
            }
            n as isize
        }
        None => -1,
    }
}
/// std::fs::remove_file — TAG_FS_UNLINK(name) to the cwd dir cap (slot 1).
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_unlink(path: *const u8, len: usize) -> i32 {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_UNLINK);
    let n = len.min(56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(path, dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    if sys_call(cwd_handle(), &mut m).is_err() || m.data[0] != 0 {
        -1
    } else {
        0
    }
}
/// std::fs::rename — TAG_FS_RENAME packs `old\0new\0` (each <=28 B) to the cwd cap.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_fs_rename(
    old: *const u8,
    old_len: usize,
    new: *const u8,
    new_len: usize,
) -> i32 {
    let mut m = MsgBuf::new(oxbow_abi::TAG_FS_RENAME);
    // Pack `old\0new\0` into the 512-byte inline data area. Cap each path at fsd's
    // PLEN (200) — far past the old 28-byte limit, so deep/tmpdir-prefixed renames work.
    let ol = old_len.min(200);
    let nl = new_len.min(200);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(old, dst, ol);
        *dst.add(ol) = 0;
        core::ptr::copy_nonoverlapping(new, dst.add(ol + 1), nl);
        *dst.add(ol + 1 + nl) = 0;
    }
    // Tell the kernel how many words carry the two NUL-terminated paths.
    m.data_len = (((ol + 1 + nl + 1) + 7) / 8) as u32;
    if sys_call(cwd_handle(), &mut m).is_err() || m.data[0] != 0 {
        -1
    } else {
        0
    }
}
/// §97b std::env::set_current_dir — re-root the cwd *capability*. `path` is the
/// std-normalized *absolute* target within this process's namespace: it always starts
/// with `/` and contains no `.`/`..` (std collapses those lexically — it cannot ascend
/// above `/`, which is the slot-1 spawn-root, matching fsd's confinement rule). We
/// resolve it relative to slot 1 (the root cap) so descent, ascent, and multi-component
/// paths all work uniformly. `/` itself resolves to slot 1. Installs the new dir cap as
/// the cwd (so later relative fs ops + child spawns follow it) and returns 0; returns -1
/// if the path can't be opened or names a non-directory. Closes the previously re-rooted
/// cap (never the original slot 1) so repeated chdirs don't leak handles.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_chdir(path: *const u8, len: usize) -> i32 {
    let p = unsafe { core::slice::from_raw_parts(path, len) };
    // Strip the leading '/': fsd resolves relative to the slot-1 root cap's subtree.
    let rel = if p.first() == Some(&b'/') { &p[1..] } else { p };
    let store = |new: u64| {
        let old = CWD_HANDLE.swap(new, core::sync::atomic::Ordering::Relaxed);
        if old != 1 {
            let _ = sys_close(old as Handle);
        }
    };
    if rel.is_empty() {
        // Target is the root of our namespace — that is slot 1 itself.
        store(1);
        return 0;
    }
    match fs::open(1 as Handle, rel) {
        Some(n) if n.kind == oxbow_abi::FS_DIR => {
            store(n.cap as u64);
            0
        }
        Some(n) => {
            // Opened, but it is a regular file, not a directory — release + reject.
            let _ = sys_close(n.cap as Handle);
            -1
        }
        None => -1,
    }
}

// §98 std::process::Command — spawn a child from ELF bytes (std reads them via
// std::fs), inheriting the parent's stdio + cwd + net caps; wait on its exit notif.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_spawn(
    elf: *const u8,
    elf_len: usize,
    argv: *const u8,
    argv_len: usize,
    stdout_cap: u32,
    pid_out: *mut u32,
) -> i64 {
    let notif = match sys_notif_create() {
        Ok(n) => n,
        Err(_) => return -1,
    };
    let mut sm = MsgBuf::new(0);
    sm.data[0] = 8 * 1024 * 1024; // child budget (covers coreutils + simple std)
    sm.data[1] = argv as u64;
    sm.data[2] = argv_len as u64;
    sm.data_len = 3;
    sm.handle_count = 4;
    sm.handles[0] = cwd_handle(); // cwd dir cap (slot 1, or the parent's re-rooted cwd)
    sm.handles[1] = stdout_cap as Handle; // stdout: 2 (inherit) or a pipe write-end
    sm.handles[2] = 4; // stdin (SPAWN_STDIN)
    sm.handles[3] = oxbow_abi::BOOT_NET_EP; // net
    let elf_slice = unsafe { core::slice::from_raw_parts(elf, elf_len) };
    match sys_spawn_bytes(elf_slice, BOOT_MEM, &sm, notif) {
        Ok(pid) => {
            unsafe { pid_out.write(pid as u32) };
            notif as i64
        }
        Err(_) => {
            let _ = sys_close(notif);
            -1
        }
    }
}
/// Block on a child's exit notification, then return its exit status.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_wait(notif: i64) -> i32 {
    let _ = sys_notif_wait(notif as Handle);
    sys_notif_status(notif as Handle)
}
/// Non-blocking child-exit check (std `Command::try_wait`). Returns the exit code if
/// the child has exited, or `i64::MIN` if it is still running. Drains the exit
/// signal, so the caller (std) caches the result — a later `__oxbow_wait` reads that
/// cache instead of blocking on a now-drained notification.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_try_wait(notif: i64) -> i64 {
    if sys_notif_poll(notif as Handle) > 0 {
        sys_notif_status(notif as Handle) as i64
    } else {
        i64::MIN
    }
}
/// std `Command::kill` — terminate the child the exit `notif` belongs to.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_kill(notif: i64, code: i32) -> i32 {
    match sys_proc_kill(notif as Handle, code) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
// §101 std::net external-TCP client shims onto the net server (NET_CTL = BOOT_NET_EP).
// std's loopback TCP is handled in-process; these back `TcpStream::connect` to a real
// (non-loopback) host via smoltcp in the net server.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_tcp_connect(ip_be: u32, port: u16) -> i64 {
    match tcp::connect(oxbow_abi::BOOT_NET_EP, ip_be.to_be_bytes(), port) {
        Some(h) => h as i64,
        None => -1,
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_tcp_connect6(addr16: *const u8, port: u16) -> i64 {
    let mut addr = [0u8; 16];
    unsafe { core::ptr::copy_nonoverlapping(addr16, addr.as_mut_ptr(), 16) };
    match tcp::connect6(oxbow_abi::BOOT_NET_EP, addr, port) {
        Some(h) => h as i64,
        None => -1,
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_tcp_send(sock: i64, buf: *const u8, len: usize) -> isize {
    // Large chunk: zero-copy via the socket's frame (up to a full MTU per IPC vs the
    // 504-byte inline cap — netmap Stage 2). On attach failure, fall through to inline.
    if len > 504 {
        if let Some(frame) = udp::attach_sock(sock as Handle) {
            let n = len.min(1472);
            unsafe { core::ptr::copy_nonoverlapping(buf, frame, n) };
            match tcp::sendv(sock as Handle, n) {
                Some(sent) => return sent as isize,
                None => return -1,
            }
        }
    }
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    // Return the ACTUAL bytes accepted (one inline message ≤504 B), so std's `write_all`
    // loops over the rest. Previously this returned `len` while the server only took the
    // first 48 bytes — silently dropping the tail of any larger write.
    match tcp::send(sock as Handle, data) {
        Some(n) => n as isize,
        None => -1,
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_tcp_recv(sock: i64, buf: *mut u8, len: usize) -> isize {
    // Large read: zero-copy via the socket's frame (up to a full MTU per IPC). n == 0
    // means the connection closed (server-side recv blocks until data or close), same
    // as the inline path. On attach failure, fall through to inline.
    if len > 504 {
        if let Some(frame) = udp::attach_sock(sock as Handle) {
            let n = tcp::recvv(sock as Handle, len);
            if n > 0 {
                unsafe { core::ptr::copy_nonoverlapping(frame as *const u8, buf, n) };
            }
            return n as isize;
        }
    }
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    tcp::recv(sock as Handle, out) as isize
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_tcp_close(sock: i64) {
    tcp::close(sock as Handle);
}
// §102 std::net wire-TcpListener shims: a real listening socket + non-blocking accept
// in the net server's smoltcp stack.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_tcp_listen(port: u16) -> i64 {
    match tcp::listen(oxbow_abi::BOOT_NET_EP, port) {
        Some(h) => h as i64,
        None => -1,
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_tcp_accept(
    listener: i64,
    peer_ip16: *mut u8,
    is_v6: *mut u8,
    peer_port: *mut u16,
) -> i64 {
    match tcp::accept(listener as Handle) {
        Some((h, addr, v6, port)) => {
            unsafe {
                core::ptr::copy_nonoverlapping(addr.as_ptr(), peer_ip16, 16);
                is_v6.write(v6 as u8);
                peer_port.write(port);
            }
            h as i64
        }
        None => -1,
    }
}
// §101 std::net external-UDP shims. std handles loopback UDP in-process; a socket that
// sends to / binds a non-loopback address gets a real net-server UDP socket via these.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_udp_bind(_ip_be: u32, port: u16, out_port: *mut u16) -> i64 {
    match udp::bind(oxbow_abi::BOOT_NET_EP, port) {
        Some((h, p)) => {
            unsafe { out_port.write(p) };
            h as i64
        }
        None => -1,
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_udp_send_to(
    sock: i64,
    ip_be: u32,
    port: u16,
    buf: *const u8,
    len: usize,
) -> isize {
    if len > 1472 {
        return -1; // one datagram, one MTU; we do not fragment
    }
    if len > 480 {
        // Large datagram: zero-copy via this socket's transfer frame (full MTU).
        if let Some(frame) = udp::attach_sock(sock as Handle) {
            unsafe { core::ptr::copy_nonoverlapping(buf, frame, len) };
            if udp::sendv(sock as Handle, ip_be.to_be_bytes(), port, len) {
                return len as isize;
            }
        }
        return -1;
    }
    let payload = unsafe { core::slice::from_raw_parts(buf, len) };
    if udp::sendto(sock as Handle, ip_be.to_be_bytes(), port, payload) {
        len as isize
    } else {
        -1
    }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_udp_recv_from(
    sock: i64,
    buf: *mut u8,
    len: usize,
    src_ip: *mut u32,
    src_port: *mut u16,
) -> isize {
    // Prefer this socket's transfer frame: the whole datagram (up to the full MTU)
    // lands in the frame, so std's recv_from never truncates at the old 480-byte inline
    // cap. Fall back to the inline path only if attach fails.
    if let Some(frame) = udp::attach_sock(sock as Handle) {
        let (n, sip, sport) = udp::recvv_src(sock as Handle);
        unsafe {
            src_ip.write(u32::from_be_bytes(sip));
            src_port.write(sport);
        }
        if n == 0 {
            return -1; // nothing buffered now; std polls
        }
        let copy = n.min(len);
        unsafe { core::ptr::copy_nonoverlapping(frame as *const u8, buf, copy) };
        return copy as isize;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    let (n, sip, sport) = udp::recvfrom_src(sock as Handle, out);
    unsafe {
        src_ip.write(u32::from_be_bytes(sip));
        src_port.write(sport);
    }
    // 0 from a non-blocking recv means "nothing buffered"; report -1 so std can poll.
    if n == 0 { -1 } else { n as isize }
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_udp_close(sock: i64) {
    udp::close(sock as Handle);
}
// §103 std::net DNS resolution: resolve a hostname to an IPv4 by querying the leased
// resolver over UDP, reusing the rt dns query/parse helpers. Writes a big-endian IPv4
// to `out_ip` and returns 0 on success; -1 on failure/timeout. (Inline UDP path — fine
// for the common single-A response; very large responses can truncate at 56 bytes.)
/// DNS is serialized with `DNS_LOCK` (binds a fresh per-socket transfer frame each
/// time); one resolver query in flight at a time keeps the socket-slot table calm.
#[cfg(feature = "hosted")]
static DNS_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
/// Send a DNS query of `qtype` for `name` over a per-socket transfer frame and copy the
/// response into `out`, returning its length (0 = fail/timeout). Serialized via
/// `DNS_LOCK`; binds a fresh socket + attaches its own frame each call.
#[cfg(feature = "hosted")]
unsafe fn dns_transport(name: &str, qtype: u16, out: &mut [u8]) -> usize {
    use core::sync::atomic::Ordering;
    while DNS_LOCK.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let server = udp::dns_server(oxbow_abi::BOOT_NET_EP);
    // Bind first, then attach THIS socket's transfer frame (per-socket — netmap Stage 2).
    let Some((sock, _)) = udp::bind(oxbow_abi::BOOT_NET_EP, 0) else {
        DNS_LOCK.store(false, Ordering::Release);
        return 0;
    };
    let Some(frame) = udp::attach_sock(sock) else {
        udp::close(sock);
        DNS_LOCK.store(false, Ordering::Release);
        return 0;
    };
    let mut got = 0;
    let query = dns::query(0x1234, name, qtype);
    let qn = query.len().min(1472);
    // Stage the query in the frame; the reply (up to ~1472 B) lands back in it.
    unsafe { core::ptr::copy_nonoverlapping(query.as_ptr(), frame, qn) };
    if udp::sendv(sock, server, 53, qn) {
        let start = sys_uptime_ms();
        loop {
            let (n, _, _) = udp::recvv_src(sock);
            if n > 0 {
                let m = n.min(out.len());
                unsafe { core::ptr::copy_nonoverlapping(frame as *const u8, out.as_mut_ptr(), m) };
                got = m;
                break;
            }
            if sys_uptime_ms().wrapping_sub(start) > 4000 {
                break; // timeout
            }
        }
    }
    udp::close(sock);
    DNS_LOCK.store(false, Ordering::Release);
    got
}

/// Resolve `name` to an IPv4 (A record); writes a big-endian IPv4 to `out_ip`.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_dns_resolve(name: *const u8, len: usize, out_ip: *mut u32) -> i32 {
    let bytes = unsafe { core::slice::from_raw_parts(name, len) };
    let Ok(name) = core::str::from_utf8(bytes) else { return -1 };
    let mut buf = [0u8; 512];
    let n = unsafe { dns_transport(name, dns::TYPE_A, &mut buf) };
    if n > 0 {
        if let Some(ip) = dns::first_a(&buf[..n]) {
            unsafe { out_ip.write(u32::from_be_bytes(ip)) };
            return 0;
        }
    }
    -1
}

/// Resolve `name` to an IPv6 (AAAA record); writes the 16 address bytes to `out_ip16`.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_dns_resolve6(name: *const u8, len: usize, out_ip16: *mut u8) -> i32 {
    let bytes = unsafe { core::slice::from_raw_parts(name, len) };
    let Ok(name) = core::str::from_utf8(bytes) else { return -1 };
    let mut buf = [0u8; 512];
    let n = unsafe { dns_transport(name, dns::TYPE_AAAA, &mut buf) };
    if n > 0 {
        if let Some(ip) = dns::first_aaaa(&buf[..n]) {
            unsafe { core::ptr::copy_nonoverlapping(ip.as_ptr(), out_ip16, 16) };
            return 0;
        }
    }
    -1
}
// §100 piped Command stdio: a pipe → a grantable write-end (R_OUT|R_GRANT) the
// child gets as stdout, and a read-end (R_IN) the parent reads.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_pipe(rend_out: *mut u32, wend_out: *mut u32) -> i32 {
    let pipe = match sys_pipe() {
        Ok(p) => p,
        Err(_) => return -1,
    };
    let wend = sys_attenuate(pipe, oxbow_abi::R_OUT | oxbow_abi::R_GRANT).unwrap_or(0);
    let rend = sys_attenuate(pipe, oxbow_abi::R_IN).unwrap_or(0);
    let _ = sys_close(pipe);
    if wend == 0 || rend == 0 {
        let _ = sys_close(wend);
        let _ = sys_close(rend);
        return -1;
    }
    unsafe {
        rend_out.write(rend);
        wend_out.write(wend);
    }
    0
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_pipe_read(pipe: u32, buf: *mut u8, len: usize) -> isize {
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    sys_pipe_read(pipe as Handle, slice) as isize
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __oxbow_pipe_write(pipe: u32, buf: *const u8, len: usize) -> isize {
    let slice = unsafe { core::slice::from_raw_parts(buf, len) };
    sys_pipe_write(pipe as Handle, slice) as isize
}
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_pipe_close(pipe: u32) {
    let _ = sys_close(pipe as Handle);
}

/// Duplicate a pipe handle: a fresh handle to the SAME pipe object (same rights), via
/// SYS_CAP_DUP. Backs std `Pipe::try_clone`. Returns the new handle, or -1 on failure.
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_pipe_dup(pipe: u32) -> i64 {
    match sys_cap_dup(pipe as Handle) {
        Ok(h) => h as i64,
        Err(_) => -1,
    }
}
// Mark the pipe's write side closed (readers drain remaining bytes, then get EOF).
// The kernel has no writer-refcount, so closing the write-end handle alone does NOT
// signal EOF — the holder must call this explicitly (mirrors the shell's $() capture).
#[cfg(feature = "hosted")]
#[unsafe(no_mangle)]
pub extern "C" fn __oxbow_pipe_eof(pipe: u32) {
    let _ = sys_pipe_eof(pipe as Handle);
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

/// Outcome of [`sys_recv_notif`]: either a message arrived (with its reply cap) or the
/// bound notification fired.
pub enum RecvNotif {
    Message(Handle),
    Notified,
}

/// Multiplexed wait: block until EITHER a message arrives on `ep` OR `notif` is
/// signalled. Lets a single-threaded server be event-driven (e.g. wake on client
/// requests AND a device IRQ notification). `timeout_ticks` (0 = none; 1 tick ≈ 10 ms)
/// also wakes with `Notified` at the deadline — a polling fallback when a device IRQ is
/// shared/throttled and its notif can't be relied on to fire.
pub fn sys_recv_notif(
    ep: Handle,
    notif: Handle,
    msg: *mut MsgBuf,
    timeout_ticks: u64,
) -> SysResult<RecvNotif> {
    let (rax, rdx) = unsafe {
        syscall4(oxbow_abi::SYS_RECV_NOTIF, ep as u64, notif as u64, msg as u64, timeout_ticks)
    };
    SysError::from_raw(rax).map(|_| {
        if rdx & oxbow_abi::RECV_NOTIF_FIRED != 0 {
            RecvNotif::Notified
        } else {
            RecvNotif::Message(rdx as Handle)
        }
    })
}

pub fn sys_call(ep: Handle, msg: *mut MsgBuf) -> SysResult {
    let (rax, _) = unsafe { syscall2(SYS_CALL, ep as u64, msg as u64) };
    SysError::from_raw(rax)
}

/// Snapshot processes into `buf` (ps). Returns the count filled. Needs PLEDGE_PROC.
pub fn sys_proc_list(buf: &mut [oxbow_abi::ProcInfo]) -> usize {
    let (rax, rdx) = unsafe {
        syscall2(oxbow_abi::SYS_PROC_LIST, buf.as_mut_ptr() as u64, buf.len() as u64)
    };
    if rax != 0 {
        0
    } else {
        rdx as usize
    }
}

/// Kill process `pid` by id (kill). Needs PLEDGE_PROC.
pub fn sys_kill(pid: u32, code: i32) -> SysResult {
    let (rax, _) = unsafe { syscall2(oxbow_abi::SYS_KILL, pid as u64, code as u64) };
    SysError::from_raw(rax)
}

/// Voluntarily reschedule (no_std). The hosted shim is `__oxbow_yield`.
pub fn sys_yield() {
    unsafe { syscall1(oxbow_abi::SYS_YIELD, 0) };
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

/// Create a shared memory region of `pages` frames from the `mem` budget (§41).
/// Returns a grantable Shm handle (passable over a channel, mappable RW).
pub fn sys_shm_create(mem: Handle, pages: u64) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall2(oxbow_abi::SYS_SHM_CREATE, mem as u64, pages) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Map every page of shm region `shm` at consecutive vaddrs from `vaddr` (RW).
/// Returns the byte size mapped.
pub fn sys_shm_map(shm: Handle, vaddr: u64) -> SysResult<u64> {
    let (rax, rdx) = unsafe { syscall2(oxbow_abi::SYS_SHM_MAP, shm as u64, vaddr) };
    SysError::from_raw(rax).map(|_| rdx)
}

/// Physical base of a CONTIGUOUS shm region (§90) — a driver hands this to its
/// device as a DMA backing (e.g. the gpu's shared-framebuffer scanout backing).
pub fn sys_shm_phys(shm: Handle) -> SysResult<u64> {
    let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_SHM_PHYS, shm as u64) };
    SysError::from_raw(rax).map(|_| rdx)
}

/// Report a handle's capability kind (CAP_CHANNEL / CAP_SHM / CAP_OTHER).
pub fn sys_cap_type(h: Handle) -> u64 {
    let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_CAP_TYPE, h as u64) };
    if rax != 0 {
        oxbow_abi::CAP_OTHER
    } else {
        rdx
    }
}

/// Duplicate an fd-passing capability (shm/channel): a fresh handle to the same
/// object with the same rights, with an independent lifetime.
pub fn sys_cap_dup(h: Handle) -> SysResult<Handle> {
    let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_CAP_DUP, h as u64) };
    SysError::from_raw(rax).map(|_| rdx as Handle)
}

/// Allocate one DMA frame from the `mem` budget, map it writable at `vaddr`, and
/// return its physical address (§19) — for a driver's ring/buffer pointers.
pub fn sys_dma_alloc(mem: Handle, vaddr: u64) -> SysResult<u64> {
    let (rax, rdx) = unsafe { syscall2(oxbow_abi::SYS_DMA_ALLOC, mem as u64, vaddr) };
    SysError::from_raw(rax).map(|_| rdx)
}

/// Allocate `pages` PHYSICALLY CONTIGUOUS DMA frames mapped at `vaddr`, returning
/// the physical base — a device handed one (addr,len) instead of a scatter-gather
/// list. Paid from the Memory budget. See `sys_dma_alloc`.
pub fn sys_dma_alloc_contig(mem: Handle, vaddr: u64, pages: u64) -> SysResult<u64> {
    let (rax, rdx) =
        unsafe { syscall3(oxbow_abi::SYS_DMA_ALLOC_CONTIG, mem as u64, vaddr, pages) };
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

/// Wall-clock time as `(epoch_seconds, nanoseconds)` from the CMOS RTC — ambient,
/// like uptime. Backs `std::time::SystemTime`.
pub fn sys_walltime() -> (u64, u64) {
    unsafe { syscall1(oxbow_abi::SYS_WALLTIME, 0) }
}

// --- §96 in-process threads + futex ----------------------------------------

/// Spawn a thread in this process at `entry` with stack pointer `user_rsp`. The
/// caller sets up the stack (including any argument). Returns the new tid.
pub unsafe fn sys_thread_spawn(entry: u64, user_rsp: u64) -> usize {
    let (tid, _) = unsafe { syscall2(oxbow_abi::SYS_THREAD_SPAWN, entry, user_rsp) };
    tid as usize
}

/// Exit the calling thread (NOT the whole process). Never returns.
pub fn sys_thread_exit() -> ! {
    unsafe {
        syscall1(oxbow_abi::SYS_THREAD_EXIT, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Futex wait: block until `*addr != expected` and a wake arrives. Returns at once
/// if `*addr` already differs (the compare-and-block that avoids lost wakeups).
pub unsafe fn sys_futex_wait(addr: *const u32, expected: u32) {
    unsafe {
        // timeout 0 = block indefinitely.
        syscall3(oxbow_abi::SYS_FUTEX_WAIT, addr as u64, expected as u64, 0);
    }
}

/// Futex wake: wake up to `count` threads blocked on `addr`. Returns how many.
pub unsafe fn sys_futex_wake(addr: *const u32, count: u32) -> usize {
    let (n, _) = unsafe { syscall2(oxbow_abi::SYS_FUTEX_WAKE, addr as u64, count as u64) };
    n as usize
}

/// Spawn a thread running `f(arg)` on the given stack region. `f` must never return
/// (it should end with `sys_thread_exit`). Returns the new tid. The argument and
/// the entry fn are stashed at the top of the stack for the trampoline.
pub unsafe fn spawn_thread(stack: &mut [u8], f: extern "C" fn(u64) -> !, arg: u64) -> usize {
    let top = (stack.as_mut_ptr() as usize + stack.len()) & !0xF;
    let sp = top - 16;
    unsafe {
        (sp as *mut u64).write(f as u64); // [sp]   = entry fn
        ((sp + 8) as *mut u64).write(arg); // [sp+8] = arg
        sys_thread_spawn(thread_trampoline as u64, sp as u64)
    }
}

/// Entry stub for `spawn_thread`: the kernel enters here with all GPRs zero and
/// `rsp` at the stashed `[fn, arg]`. Load them and tail-call `f(arg)`.
#[unsafe(naked)]
extern "C" fn thread_trampoline() -> ! {
    core::arch::naked_asm!(
        "mov rax, [rsp]",     // entry fn
        "mov rdi, [rsp + 8]", // arg
        "and rsp, -16",       // SysV stack alignment for the call
        "call rax",           // f(arg) — must not return
        "ud2",
    );
}

/// `(used_kib, total_kib)` of the kernel's managed physical region — ambient, for
/// a system monitor. Not a capability; every process may read it.
pub fn sys_meminfo() -> (u64, u64) {
    let (_, rdx) = unsafe { syscall1(oxbow_abi::SYS_MEMINFO, 0) };
    (rdx >> 32, rdx & 0xffff_ffff)
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

/// This process's inherited identity record (§24): who we are for `whoami`,
/// `getpwnam`, and POSIX compat. The kernel mapped it read-only at SPAWN_IDENT;
/// a zeroed page (no identity passed) reads as root. DESCRIPTIVE only — it grants
/// nothing; authority is the capabilities we hold.
pub fn identity() -> &'static oxbow_abi::IdentRec {
    // SAFETY: the kernel maps a zero-filled, IdentRec-sized page here on every
    // spawn (page-aligned, so well-aligned for IdentRec's u32 fields).
    unsafe { &*(oxbow_abi::SPAWN_IDENT as *const oxbow_abi::IdentRec) }
}

pub fn uid() -> u32 {
    identity().uid
}

pub fn gid() -> u32 {
    identity().gid
}

/// The login name, defaulting to `root` when the record carries no name.
pub fn user_name() -> &'static [u8] {
    let n = identity().name_bytes();
    if n.is_empty() {
        b"root"
    } else {
        n
    }
}

/// The home directory, defaulting to `/` when the record carries no home.
pub fn home() -> &'static [u8] {
    let h = identity().home_bytes();
    if h.is_empty() {
        b"/"
    } else {
        h
    }
}

/// Point a spawn `MsgBuf` at `id` so the child inherits it (§24). `id` must stay
/// alive (its address is read by the kernel) until `sys_spawn` returns.
pub fn msg_set_identity(msg: &mut MsgBuf, id: &oxbow_abi::IdentRec) {
    msg.data[3] = id as *const oxbow_abi::IdentRec as u64;
    msg.data[4] = core::mem::size_of::<oxbow_abi::IdentRec>() as u64;
    if msg.data_len < 5 {
        msg.data_len = 5;
    }
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

/// Non-blocking drain of `notif`'s latched signal count (0 if none) — for a loop
/// that can't park on `sys_notif_wait` (the gpu's present loop polling for a
/// virtio-gpu config-change IRQ).
pub fn sys_notif_poll(notif: Handle) -> u64 {
    let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_NOTIF_POLL, notif as u64) };
    SysError::from_raw(rax).map(|_| rdx).unwrap_or(0)
}

/// §103: kill the child whose exit notification is `notif` (with exit `code`).
/// Authority is holding `notif` (the spawn-time lifecycle handle).
pub fn sys_proc_kill(notif: Handle, code: i32) -> SysResult {
    let (rax, _) = unsafe { syscall2(oxbow_abi::SYS_PROC_KILL, notif as u64, code as u64) };
    SysError::from_raw(rax).map(|_| ())
}

/// Read the last exit code delivered to `notif` (§81), non-blocking. Call right
/// after `sys_notif_wait` returns for a child you spawned, to branch on its exit
/// status (the shell's `&&`/`||`). Returns 0 if nothing recorded.
pub fn sys_notif_status(notif: Handle) -> i32 {
    let (rax, rdx) = unsafe { syscall1(SYS_NOTIF_STATUS, notif as u64) };
    if SysError::from_raw(rax).is_ok() {
        rdx as i32
    } else {
        0
    }
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

#[cfg(not(feature = "hosted"))] // §95: std supplies the panic handler when hosted
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
    use core::hint::spin_loop;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use oxbow_abi::{
        MsgBuf, PROT_READ, PROT_WRITE, TAG_NET_DNS, TAG_UDP_ATTACH, TAG_UDP_BIND, TAG_UDP_CLOSE,
        TAG_UDP_RECVFROM, TAG_UDP_RECVV, TAG_UDP_SENDTO, TAG_UDP_SENDV,
    };

    /// Client-side base vaddr of the per-socket UDP transfer frames (netmap Stage 2).
    /// Socket `sid`'s frame maps at `UDP_XFER + sid*4096`; the server returns `sid` on
    /// attach so the vaddr is stable across re-attach of a reused socket slot (the same
    /// physical page), which means we never remap (there is no frame_unmap).
    pub const UDP_XFER: u64 = 0x3E00_0000;

    const UDP_FRAME_SLOTS: usize = 16;
    /// Lock guarding the attach slow-path (the cache mutation + the one IPC).
    static FRAME_LOCK: AtomicBool = AtomicBool::new(false);
    /// Cache: socket cap -> client frame ptr, so send/recv don't re-attach per packet
    /// (0 = empty slot).
    static FRAME_SOCK: [AtomicU32; UDP_FRAME_SLOTS] =
        [const { AtomicU32::new(0) }; UDP_FRAME_SLOTS];
    static FRAME_PTR: [AtomicU64; UDP_FRAME_SLOTS] =
        [const { AtomicU64::new(0) }; UDP_FRAME_SLOTS];
    /// Per-sid one-time mapping record (0 = not yet mapped). A reused sid reuses the
    /// same page, so we map exactly once per sid over the process lifetime.
    static SID_MAPPED: [AtomicU64; UDP_FRAME_SLOTS + 1] =
        [const { AtomicU64::new(0) }; UDP_FRAME_SLOTS + 1];

    /// Attach socket `sock`'s shared transfer frame and return its client pointer,
    /// caching it so subsequent send/recv are a single IPC with zero payload copy
    /// across the boundary. Attaching via the SOCKET cap (badge = socket id) means the
    /// net server hands each socket its own page — correct per-process isolation.
    /// Returns `None` if the server can't allocate/return a frame (caller falls back
    /// to the inline path).
    pub fn attach_sock(sock: Handle) -> Option<*mut u8> {
        // Fast path: already cached.
        for i in 0..UDP_FRAME_SLOTS {
            if FRAME_SOCK[i].load(Ordering::Acquire) == sock {
                let p = FRAME_PTR[i].load(Ordering::Acquire);
                if p != 0 {
                    return Some(p as *mut u8);
                }
            }
        }
        while FRAME_LOCK.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        let mut result = None;
        'done: {
            // Re-check under the lock (another thread may have attached meanwhile).
            for i in 0..UDP_FRAME_SLOTS {
                if FRAME_SOCK[i].load(Ordering::Relaxed) == sock {
                    let p = FRAME_PTR[i].load(Ordering::Relaxed);
                    if p != 0 {
                        result = Some(p as *mut u8);
                        break 'done;
                    }
                }
            }
            let mut m = MsgBuf::new(TAG_UDP_ATTACH);
            if sys_call(sock, &mut m).is_err() || m.data[0] != 0 || m.handle_count == 0 {
                break 'done;
            }
            let frame = m.handles[0];
            let sid = m.data[1] as usize;
            if sid == 0 || sid > UDP_FRAME_SLOTS {
                break 'done;
            }
            let vaddr = UDP_XFER + (sid as u64) * 4096;
            if SID_MAPPED[sid].load(Ordering::Relaxed) == 0 {
                if sys_frame_map(frame, vaddr, PROT_READ | PROT_WRITE).is_err() {
                    break 'done;
                }
                SID_MAPPED[sid].store(vaddr, Ordering::Relaxed);
            }
            // Cache sock -> vaddr for the per-packet fast path.
            for i in 0..UDP_FRAME_SLOTS {
                if FRAME_SOCK[i].load(Ordering::Relaxed) == 0 {
                    FRAME_PTR[i].store(vaddr, Ordering::Relaxed);
                    FRAME_SOCK[i].store(sock, Ordering::Release);
                    break;
                }
            }
            result = Some(vaddr as *mut u8);
        }
        FRAME_LOCK.store(false, Ordering::Release);
        result
    }

    /// Drop a socket's cache entry on close so a future bind that reuses the cap value
    /// re-attaches cleanly (the underlying per-sid mapping is intentionally kept).
    /// `pub` because the TCP path shares this per-socket frame machinery.
    pub fn frame_drop(sock: Handle) {
        while FRAME_LOCK.swap(true, Ordering::Acquire) {
            spin_loop();
        }
        for i in 0..UDP_FRAME_SLOTS {
            if FRAME_SOCK[i].load(Ordering::Relaxed) == sock {
                FRAME_SOCK[i].store(0, Ordering::Relaxed);
                FRAME_PTR[i].store(0, Ordering::Relaxed);
            }
        }
        FRAME_LOCK.store(false, Ordering::Release);
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

    /// Non-blocking: receive the next datagram for `sock` INTO its shared frame.
    /// Returns `(len, src_ip, src_port)` (len 0 = nothing buffered now). The whole
    /// datagram (up to the full MTU) lands in the frame — no inline-size truncation.
    /// Requires a prior `attach_sock`.
    pub fn recvv_src(sock: Handle) -> (usize, [u8; 4], u16) {
        let mut m = MsgBuf::new(TAG_UDP_RECVV);
        if sys_call(sock, &mut m).is_err() {
            return (0, [0; 4], 0);
        }
        let n = (m.data[0] as usize).min(1472);
        let sip = (m.data[1] as u32).to_be_bytes();
        let sport = m.data[2] as u16;
        (n, sip, sport)
    }

    /// Close a UDP socket: free the net server's socket slot AND the client cap.
    /// Always use this (not a bare sys_close) — the net slot table is small and a
    /// bind without a matching close leaks a slot. One-way SEND (we don't need the
    /// reply): the net server is single-threaded and frees the slot by the message's
    /// badge, so this is processed before any later bind that could reuse the slot.
    pub fn close(sock: Handle) {
        let m = MsgBuf::new(TAG_UDP_CLOSE);
        let _ = crate::sys_send(sock, &m);
        frame_drop(sock);
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
    /// Send one UDP datagram (≤480 payload bytes — the inline message area past the
    /// addr/port/len header). A datagram cannot be split, so an oversized payload is
    /// REJECTED (returns false) rather than silently truncated. Sends the WHOLE payload
    /// for sizes that fit.
    pub fn sendto(sock: Handle, ip: [u8; 4], dport: u16, payload: &[u8]) -> bool {
        if payload.len() > 480 {
            return false; // too large for one inline datagram; don't truncate
        }
        let n = payload.len();
        let mut m = MsgBuf::new(TAG_UDP_SENDTO);
        m.data[0] = u32::from_be_bytes(ip) as u64;
        m.data[1] = dport as u64;
        m.data[2] = n as u64;
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(payload.as_ptr(), dst.add(24), n) };
        m.data_len = 64; // up to the full 512-byte inline area
        sys_call(sock, &mut m).is_ok() && m.data[0] == 0
    }

    /// Receive a datagram on `sock` into `out` (blocks); returns payload length. Up to
    /// 480 bytes ride one inline reply (was 56, which truncated larger datagrams).
    pub fn recvfrom(sock: Handle, out: &mut [u8]) -> usize {
        let mut m = MsgBuf::new(TAG_UDP_RECVFROM);
        if sys_call(sock, &mut m).is_err() {
            return 0;
        }
        let n = (m.data[0] as usize).min(out.len()).min(480);
        let src = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), n) };
        out[..n].copy_from_slice(src);
        n
    }

    /// Like [`recvfrom`] but also returns the sender's IPv4 + port. The net server packs
    /// them in the LAST two message words (clear of the up-to-480-byte payload) — §101.
    pub fn recvfrom_src(sock: Handle, out: &mut [u8]) -> (usize, [u8; 4], u16) {
        let mut m = MsgBuf::new(TAG_UDP_RECVFROM);
        if sys_call(sock, &mut m).is_err() {
            return (0, [0; 4], 0);
        }
        let n = (m.data[0] as usize).min(out.len()).min(480);
        let src = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), n) };
        out[..n].copy_from_slice(src);
        let sip = (m.data[oxbow_abi::MSG_DATA_WORDS - 2] as u32).to_be_bytes();
        let sport = m.data[oxbow_abi::MSG_DATA_WORDS - 1] as u16;
        (n, sip, sport)
    }
}

/// Per-socket batched ring + doorbell (netmap Stage 3). Queue K datagrams into the
/// TX ring with plain memory writes, then `kick` ONCE — the server drains the whole
/// batch and harvests buffered RX in a single domain crossing (vs one crossing per
/// packet). For the small-packet / high-pps case; large datagrams use `udp::sendv`.
pub mod ring {
    use crate::{sys_call, sys_frame_map, Handle};
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use oxbow_abi::{
        MsgBuf, PROT_READ, PROT_WRITE, RING_HDR_BYTES, RING_PAYLOAD_MAX, RING_SLOTS,
        RING_SLOT_STRIDE, TAG_UDP_KICK, TAG_UDP_RING, TAG_UDP_RXNOTIF,
    };

    /// Client-side base vaddr of the per-socket ring pages (1 MiB above `UDP_XFER`).
    pub const RING_XFER: u64 = 0x3E10_0000;
    /// Per-sid one-time mapping record (a reused sid re-shares the same page → map once).
    static RING_MAPPED: [AtomicU64; (RING_SLOTS as usize) * 2 + 1] =
        [const { AtomicU64::new(0) }; (RING_SLOTS as usize) * 2 + 1];

    /// A mapped per-socket ring: `push` to queue TX, `kick` the doorbell, `pop` RX.
    pub struct Ring {
        base: u64,
        sock: Handle,
    }

    /// Attach socket `sock`'s batch ring (maps the ring page once per sid). `None` if
    /// the server can't provide one (e.g. not a UDP socket).
    pub fn attach(sock: Handle) -> Option<Ring> {
        let mut m = MsgBuf::new(TAG_UDP_RING);
        if sys_call(sock, &mut m).is_err() || m.data[0] != 0 || m.handle_count == 0 {
            return None;
        }
        let frame = m.handles[0];
        let sid = m.data[1] as usize;
        if sid == 0 || sid >= RING_MAPPED.len() {
            return None;
        }
        let vaddr = RING_XFER + sid as u64 * 4096;
        if RING_MAPPED[sid].load(Ordering::Acquire) == 0 {
            if sys_frame_map(frame, vaddr, PROT_READ | PROT_WRITE).is_err() {
                return None;
            }
            RING_MAPPED[sid].store(vaddr, Ordering::Release);
        }
        Some(Ring { base: vaddr, sock })
    }

    impl Ring {
        fn idx(&self, off: u64) -> &AtomicU32 {
            unsafe { &*((self.base + off) as *const AtomicU32) }
        }
        fn slot(&self, i: u32) -> u64 {
            self.base + RING_HDR_BYTES as u64 + i as u64 * RING_SLOT_STRIDE as u64
        }

        /// Queue one datagram into the TX ring — a plain memory write, NO IPC. Returns
        /// false if the ring is full (`kick` to drain). Payload truncated to the slot.
        pub fn push(&self, ip: [u8; 4], port: u16, payload: &[u8]) -> bool {
            let tx_tail = self.idx(4).load(Ordering::Relaxed);
            let tx_head = self.idx(0).load(Ordering::Acquire);
            let next = (tx_tail + 1) % RING_SLOTS;
            if next == tx_head {
                return false; // full
            }
            let n = payload.len().min(RING_PAYLOAD_MAX);
            let slot = self.slot(tx_tail);
            unsafe {
                (slot as *mut u32).write_unaligned(u32::from_be_bytes(ip));
                ((slot + 4) as *mut u16).write_unaligned(port);
                ((slot + 6) as *mut u16).write_unaligned(n as u16);
                core::ptr::copy_nonoverlapping(payload.as_ptr(), (slot + 8) as *mut u8, n);
            }
            self.idx(4).store(next, Ordering::Release); // publish to the server
            true
        }

        /// Register an async RX notification (Stage 3): pass a `notif` cap (from
        /// `sys_notif_create`) the server will signal when a datagram lands for this
        /// socket while it's idle. After this, queue TX + `kick`, then block in
        /// `sys_notif_wait(notif)` and `pop` on wake — no busy poll-kick. Returns true
        /// if the server accepted it.
        pub fn set_rxnotif(&self, notif: Handle) -> bool {
            let mut m = MsgBuf::new(TAG_UDP_RXNOTIF);
            m.handles[0] = notif;
            m.handle_count = 1;
            crate::sys_call(self.sock, &mut m).is_ok() && m.data[0] == 0
        }

        /// Ring the doorbell: the server drains every posted TX slot and harvests any
        /// buffered RX into the RX ring. Returns `(sent, harvested)`. ONE crossing.
        pub fn kick(&self) -> (usize, usize) {
            let mut m = MsgBuf::new(TAG_UDP_KICK);
            if sys_call(self.sock, &mut m).is_err() {
                return (0, 0);
            }
            (m.data[0] as usize, m.data[1] as usize)
        }

        /// Pop one received datagram from the RX ring into `out`. Returns
        /// `(src_ip, src_port, len)`, or `None` when the RX ring is empty.
        pub fn pop(&self, out: &mut [u8]) -> Option<([u8; 4], u16, usize)> {
            let rx_head = self.idx(8).load(Ordering::Relaxed);
            let rx_tail = self.idx(12).load(Ordering::Acquire);
            if rx_head == rx_tail {
                return None; // empty
            }
            let slot = self.slot(RING_SLOTS + rx_head);
            let (ip, port, len) = unsafe {
                let ip = (slot as *const u32).read_unaligned();
                let port = ((slot + 4) as *const u16).read_unaligned();
                let len = ((slot + 6) as *const u16).read_unaligned() as usize;
                (ip, port, len)
            };
            let n = len.min(RING_PAYLOAD_MAX).min(out.len());
            unsafe {
                core::ptr::copy_nonoverlapping((slot + 8) as *const u8, out.as_mut_ptr(), n);
            }
            self.idx(8).store((rx_head + 1) % RING_SLOTS, Ordering::Release); // free slot
            Some((ip.to_be_bytes(), port, n))
        }
    }
}

/// Minimal DNS A-record client: build a recursive query, parse the first answer.
pub mod dns {
    use alloc::vec::Vec;

    pub const TYPE_A: u16 = 1;
    pub const TYPE_AAAA: u16 = 28;

    /// Build a standard recursive query for `name` of record type `qtype` (A or AAAA).
    pub fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
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
        q.extend_from_slice(&qtype.to_be_bytes()); // QTYPE
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

    /// Parse the first AAAA (IPv6) answer out of a DNS response.
    pub fn first_aaaa(resp: &[u8]) -> Option<[u8; 16]> {
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
            if typ == TYPE_AAAA && rdlen == 16 && off + 16 <= resp.len() {
                let mut a = [0u8; 16];
                a.copy_from_slice(&resp[off..off + 16]);
                return Some(a);
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
    use oxbow_abi::{
        MsgBuf, TAG_TCP_ACCEPT, TAG_TCP_CLOSE, TAG_TCP_CONNECT, TAG_TCP_CONNECT6, TAG_TCP_LISTEN,
        TAG_TCP_RECV, TAG_TCP_RECVV, TAG_TCP_SEND, TAG_TCP_SENDV,
    };

    /// Open an IPv6 TCP connection to `addr:port`; returns a socket cap once the
    /// handshake completes (None on refusal/timeout).
    pub fn connect6(ctl: Handle, addr: [u8; 16], port: u16) -> Option<Handle> {
        let mut m = MsgBuf::new(TAG_TCP_CONNECT6);
        m.data[0] = port as u64;
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(addr.as_ptr(), dst.add(8), 16) };
        m.data_len = 3;
        if sys_call(ctl, &mut m).is_err() || m.data[0] != 0 || m.handle_count == 0 {
            return None;
        }
        Some(m.handles[0])
    }

    /// Start listening on `port` via control cap `ctl`; returns a badged listener cap.
    pub fn listen(ctl: Handle, port: u16) -> Option<Handle> {
        let mut m = MsgBuf::new(TAG_TCP_LISTEN);
        m.data[0] = port as u64;
        m.data_len = 1;
        if sys_call(ctl, &mut m).is_err() || m.data[0] != 0 || m.handle_count == 0 {
            return None;
        }
        Some(m.handles[0])
    }

    /// Non-blocking accept on a listener cap: returns (socket cap, peer addr (16 bytes;
    /// v4 in the first 4), is_v6, peer port) when a connection is pending, else None.
    pub fn accept(listener: Handle) -> Option<(Handle, [u8; 16], bool, u16)> {
        let mut m = MsgBuf::new(TAG_TCP_ACCEPT);
        if sys_call(listener, &mut m).is_err() || m.data[0] != 0 || m.handle_count == 0 {
            return None;
        }
        let is_v6 = m.data[1] == 6;
        let port = m.data[2] as u16;
        let mut addr = [0u8; 16];
        let src = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(24), 16) };
        addr.copy_from_slice(src);
        Some((m.handles[0], addr, is_v6, port))
    }

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

    /// Send up to 504 bytes (one inline message) on a TCP socket cap. Returns the number
    /// of bytes the server accepted (may be < offered if its send buffer is near full),
    /// or None on error. Callers must loop to send the rest.
    pub fn send(sock: Handle, data: &[u8]) -> Option<usize> {
        let n = data.len().min(504);
        let mut m = MsgBuf::new(TAG_TCP_SEND);
        m.data[0] = n as u64;
        let dst = m.data.as_mut_ptr() as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst.add(8), n) };
        m.data_len = 64; // up to all 512 inline bytes carry the count + payload
        if sys_call(sock, &mut m).is_err() || m.data[0] != 0 {
            return None;
        }
        Some(m.data[1] as usize)
    }

    /// Receive on a TCP socket cap into `out` (blocks server-side until data or
    /// close); returns the byte count (0 = connection closed).
    pub fn recv(sock: Handle, out: &mut [u8]) -> usize {
        let mut m = MsgBuf::new(TAG_TCP_RECV);
        // Tell the server how many bytes we can take, so it consumes only that much
        // from the TCP stream — a smaller read must NOT drop the rest (TLS reads 5-byte
        // record headers; dropped bytes corrupt the stream). Up to 504 bytes ride one
        // inline reply (payload past the count word), ~9x fewer round trips than 56.
        m.data[0] = out.len().min(504) as u64;
        m.data_len = 1;
        if sys_call(sock, &mut m).is_err() {
            return 0;
        }
        let n = (m.data[0] as usize).min(out.len()).min(504);
        let src = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), n) };
        out[..n].copy_from_slice(src);
        n
    }

    /// Send the first `len` bytes (≤1472) of the socket's transfer frame on `sock`
    /// (netmap Stage 2 — zero-copy stream chunk). Returns the bytes smoltcp accepted
    /// (may be < len if its tx buffer is full), or None if the socket can't send.
    /// Requires a prior `crate::udp::attach_sock(sock)`.
    pub fn sendv(sock: Handle, len: usize) -> Option<usize> {
        let mut m = MsgBuf::new(TAG_TCP_SENDV);
        m.data[0] = len.min(1472) as u64;
        m.data_len = 1;
        if sys_call(sock, &mut m).is_err() || m.data[0] != 0 {
            return None;
        }
        Some(m.data[1] as usize)
    }

    /// Receive up to `want` (≤1472) bytes on `sock` INTO the socket's transfer frame;
    /// returns the byte count (0 = closed/none). Requires a prior `attach_sock`.
    pub fn recvv(sock: Handle, want: usize) -> usize {
        let mut m = MsgBuf::new(TAG_TCP_RECVV);
        m.data[0] = want.clamp(1, 1472) as u64;
        m.data_len = 1;
        if sys_call(sock, &mut m).is_err() {
            return 0;
        }
        (m.data[0] as usize).min(want).min(1472)
    }

    /// Close a TCP socket cap and release the client handle. One-way SEND (reply unused):
    /// a preceding send() CALL has already drained the data to the wire, and the net
    /// server (single-threaded, frees the slot by badge) will run smoltcp's close → FIN
    /// asynchronously. Returns as soon as the close is received, not after it's processed.
    pub fn close(sock: Handle) {
        let m = MsgBuf::new(TAG_TCP_CLOSE);
        let _ = crate::sys_send(sock, &m);
        crate::udp::frame_drop(sock); // shared per-socket frame cache
        let _ = sys_close(sock);
    }
}

/// Bidirectional byte+capability channels (§40): the socketpair/SCM_RIGHTS
/// primitive that local protocols (e.g. Wayland) run over. Both ends are
/// Channel handles; either can stream bytes and pass capabilities to the peer.
pub mod channel {
    use crate::{syscall1, syscall2, syscall3, syscall5, Handle};
    use oxbow_abi::{
        CHAN_NONBLOCK, SYS_CHAN_WAIT, SYS_CHANNEL_CLOSE, SYS_CHANNEL_PAIR, SYS_CHANNEL_RECV,
        SYS_CHANNEL_SEND,
    };

    /// Create a connected pair; returns both ends `(h0, h1)` in this process.
    pub fn pair() -> Option<(Handle, Handle)> {
        let (rax, rdx) = unsafe { syscall1(SYS_CHANNEL_PAIR, 0) };
        if rax != 0 {
            return None;
        }
        Some(((rdx & 0xffff_ffff) as Handle, (rdx >> 32) as Handle))
    }

    /// Send `bytes` (all of them, blocking while full) plus the capabilities in
    /// `caps`. Returns bytes sent (0 if the peer is gone).
    pub fn send(h: Handle, bytes: &[u8], caps: &[Handle]) -> usize {
        let (rax, rdx) = unsafe {
            syscall5(
                SYS_CHANNEL_SEND,
                h as u64,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                caps.as_ptr() as u64,
                caps.len() as u64,
            )
        };
        if rax != 0 {
            0
        } else {
            rdx as usize
        }
    }

    /// Receive into `buf`, delivering up to `caps_out.len()` capabilities (their
    /// handles written into `caps_out`). Returns `(nbytes, ncaps)`; `(0, 0)` on
    /// EOF. With `nonblock`, returns `None` if nothing is buffered.
    pub fn recv(
        h: Handle,
        buf: &mut [u8],
        caps_out: &mut [Handle],
        nonblock: bool,
    ) -> Option<(usize, usize)> {
        let flags = if nonblock { CHAN_NONBLOCK } else { 0 };
        let packed = (caps_out.len() as u64) | (flags << 32);
        let (rax, rdx) = unsafe {
            syscall5(
                SYS_CHANNEL_RECV,
                h as u64,
                buf.as_mut_ptr() as u64,
                buf.len() as u64,
                caps_out.as_mut_ptr() as u64,
                packed,
            )
        };
        if rax != 0 {
            return None; // WouldBlock (nonblocking)
        }
        Some(((rdx & 0xffff_ffff) as usize, (rdx >> 32) as usize))
    }

    /// Close this end; the peer observes EOF.
    pub fn close(h: Handle) {
        let _ = unsafe { syscall1(SYS_CHANNEL_CLOSE, h as u64) };
    }

    /// Block until at least one of `handles` (channel caps) is readable/at EOF, or
    /// `timeout_ms` elapses (0 = wait forever). The kernel parks us on all of them
    /// and sleeps; a send into any — or the timer deadline — wakes us. This is what
    /// a blocking `epoll_wait`/`poll` sleeps on instead of busy-polling.
    pub fn wait(handles: &[u32], timeout_ms: u64) {
        unsafe {
            syscall3(SYS_CHAN_WAIT, handles.as_ptr() as u64, handles.len() as u64, timeout_ms)
        };
    }

    /// Non-blocking readiness bits: 1=readable, 2=eof, 4=writable (for epoll/poll).
    pub fn poll(h: Handle) -> u64 {
        let (rax, rdx) = unsafe { syscall1(oxbow_abi::SYS_CHANNEL_POLL, h as u64) };
        if rax != 0 {
            0b011 // error => treat as readable+EOF so callers progress/close
        } else {
            rdx
        }
    }
}
