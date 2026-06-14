//! cc-hello — proof that a real C program, compiled by clang, runs on oxbow.
//!
//! The Rust side is the runtime + a minimal libc: `oxbow_main` (oxbow-rt's entry)
//! calls the C `main`, and the `extern "C"` functions below back the C program's
//! `puts`/`printf`/`write` with oxbow's capability syscalls (stdout = the tty
//! endpoint the spawner granted). `src/hello.c` is compiled + linked in by
//! build.rs. This is the seed of oxbow-libc.
#![no_std]
#![no_main]
#![feature(c_variadic)]

extern crate alloc;

use core::ptr::addr_of_mut;
use oxbow_abi::{Handle, MsgBuf, BOOT_EP, FS_FILE, TAG_FS_READ};
use oxbow_rt as rt;

extern "C" {
    fn main(argc: i32, argv: *const *const u8) -> i32;
}

// --- POSIX file descriptors over capabilities -----------------------------
// fds 0/1/2 are stdin/stdout/stderr (the tty); 3.. index this table, each slot
// holding the fs file capability `open` was given plus a read offset.
#[derive(Clone, Copy)]
struct FdSlot {
    handle: Handle,
    off: u64,
    used: bool,
}
const MAX_FD: usize = 16;
static mut FDS: [FdSlot; MAX_FD] = [FdSlot { handle: 0, off: 0, used: false }; MAX_FD];

/// One fs READ at `off` on a file capability; returns bytes copied (<=56).
unsafe fn fs_read(cap: Handle, off: u64, out: &mut [u8]) -> usize {
    let mut m = MsgBuf::new(TAG_FS_READ);
    m.data[0] = off;
    m.data_len = 1;
    if rt::sys_call(cap, &mut m).is_err() {
        return 0;
    }
    let count = (m.data[0] as usize).min(out.len()).min(56);
    let src = (m.data.as_ptr() as *const u8).add(8);
    core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), count);
    count
}

/// `open(path, flags)` — resolve `path` relative to the directory capability the
/// spawner granted us (BOOT_EP = the shell's cwd), read-only. Returns an fd.
#[no_mangle]
pub unsafe extern "C" fn open(path: *const u8, _flags: i32) -> i32 {
    let n = cstr_len(path);
    if n == 0 {
        return -1;
    }
    let p = core::slice::from_raw_parts(path, n);
    match rt::fs::open(BOOT_EP, p) {
        Some(node) if node.kind == FS_FILE => {
            for i in 3..MAX_FD {
                let slot = &mut (*addr_of_mut!(FDS))[i];
                if !slot.used {
                    *slot = FdSlot { handle: node.cap, off: 0, used: true };
                    return i as i32;
                }
            }
            let _ = rt::sys_close(node.cap);
            -1
        }
        Some(node) => {
            let _ = rt::sys_close(node.cap);
            -1 // not a regular file
        }
        None => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: i32) -> i32 {
    if fd < 3 || fd as usize >= MAX_FD {
        return 0; // 0/1/2 (tty) are not ours to close
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if slot.used {
        let _ = rt::sys_close(slot.handle);
        slot.used = false;
    }
    0
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let code = unsafe { main(0, core::ptr::null()) };
    rt::sys_exit(code as i64 as u64);
}

unsafe fn out(s: &[u8]) {
    rt::stdout_write(s);
}

#[no_mangle]
pub unsafe extern "C" fn write(fd: i32, buf: *const u8, len: usize) -> isize {
    if buf.is_null() {
        return -1;
    }
    if fd == 1 || fd == 2 {
        out(core::slice::from_raw_parts(buf, len));
        len as isize
    } else {
        -1
    }
}

/// `read(fd, buf, len)` — one read from a file fd (may return fewer bytes than
/// requested, as POSIX allows; the caller loops until 0 = EOF).
#[no_mangle]
pub unsafe extern "C" fn read(fd: i32, buf: *mut u8, len: usize) -> isize {
    if buf.is_null() || fd < 3 || fd as usize >= MAX_FD {
        return -1;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if !slot.used {
        return -1;
    }
    let mut tmp = [0u8; 56];
    let want = len.min(tmp.len());
    let got = fs_read(slot.handle, slot.off, &mut tmp[..want]);
    core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf, got);
    slot.off += got as u64;
    got as isize
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const u8) -> i32 {
    let n = cstr_len(s);
    out(core::slice::from_raw_parts(s, n));
    out(b"\n");
    0
}

#[no_mangle]
pub extern "C" fn exit(code: i32) -> ! {
    rt::sys_exit(code as i64 as u64);
}

// --- <stdlib.h>: malloc/free over oxbow-rt's slab heap. We stash the
// allocation size in a 16-byte header so `free` (which gets no size in C) can
// reconstruct the Layout. ---
const HDR: usize = 16;

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let total = size + HDR;
    let layout = core::alloc::Layout::from_size_align(total, 16).unwrap();
    let p = alloc::alloc::alloc(layout);
    if p.is_null() {
        return p;
    }
    *(p as *mut usize) = total;
    p.add(HDR)
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let base = ptr.sub(HDR);
    let total = *(base as *const usize);
    let layout = core::alloc::Layout::from_size_align(total, 16).unwrap();
    alloc::alloc::dealloc(base, layout);
}

#[no_mangle]
pub unsafe extern "C" fn calloc(n: usize, size: usize) -> *mut u8 {
    let total = n.saturating_mul(size);
    let p = malloc(total);
    if !p.is_null() {
        core::ptr::write_bytes(p, 0, total);
    }
    p
}

// --- <string.h>: the bits compiler-builtins doesn't already provide
// (memcpy/memset/memmove/memcmp come from compiler_builtins). ---
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    cstr_len(s)
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0usize;
    loop {
        let (ca, cb) = (*a.add(i), *b.add(i));
        if ca != cb {
            return ca as i32 - cb as i32;
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
}

unsafe fn cstr_len(s: *const u8) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

unsafe fn print_uint(mut v: u64, base: u64) -> i32 {
    if v == 0 {
        out(b"0");
        return 1;
    }
    let digits = b"0123456789abcdef";
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = digits[(v % base) as usize];
        v /= base;
    }
    out(&tmp[i..]);
    (tmp.len() - i) as i32
}

unsafe fn print_int(v: i64) -> i32 {
    if v < 0 {
        out(b"-");
        1 + print_uint((v as i128).unsigned_abs() as u64, 10)
    } else {
        print_uint(v as u64, 10)
    }
}

unsafe fn print_cstr(p: *const u8) -> i32 {
    if p.is_null() {
        out(b"(null)");
        return 6;
    }
    let n = cstr_len(p);
    out(core::slice::from_raw_parts(p, n));
    n as i32
}

/// A minimal `printf`: handles `%d`/`%i`, `%u`, `%x`, `%c`, `%s`, `%%`.
#[no_mangle]
pub unsafe extern "C" fn printf(fmt: *const u8, mut args: ...) -> i32 {
    if fmt.is_null() {
        return 0;
    }
    let mut i = 0usize;
    let mut w = 0i32;
    loop {
        let c = *fmt.add(i);
        if c == 0 {
            break;
        }
        if c == b'%' {
            i += 1;
            let spec = *fmt.add(i);
            match spec {
                b'd' | b'i' => w += print_int(args.next_arg::<i32>() as i64),
                b'u' => w += print_uint(args.next_arg::<u32>() as u64, 10),
                b'x' => w += print_uint(args.next_arg::<u32>() as u64, 16),
                b'c' => {
                    out(&[args.next_arg::<i32>() as u8]);
                    w += 1;
                }
                b's' => w += print_cstr(args.next_arg::<*const u8>()),
                b'%' => {
                    out(b"%");
                    w += 1;
                }
                0 => break,
                _ => {
                    out(&[b'%', spec]);
                    w += 2;
                }
            }
            i += 1;
        } else {
            out(&[c]);
            w += 1;
            i += 1;
        }
    }
    w
}
