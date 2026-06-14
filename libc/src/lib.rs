//! oxbow-libc — a minimal C library for oxbow, mapping POSIX onto capabilities.
//!
//! A C program crate depends on this, ships its own `.c` (compiled by build.rs),
//! and gets: `_start` (via oxbow-rt) → `oxbow_main` (here, sets up argv + stdio)
//! → the C `main`. The `extern "C"` functions below back the C standard library
//! over oxbow's syscalls — stdout is the tty endpoint the spawner granted, files
//! are opened through the directory capability passed at `BOOT_EP`. No ambient
//! authority: a C program can only touch what it was handed.
#![no_std]
#![feature(c_variadic)]

extern crate alloc;

use alloc::vec::Vec;
use core::ffi::VaList;
use core::ptr::addr_of_mut;
use oxbow_abi::{Handle, MsgBuf, BOOT_EP, FS_FILE, TAG_FS_READ};
use oxbow_rt as rt;

extern "C" {
    fn main(argc: i32, argv: *const *const u8) -> i32;
}

// ===========================================================================
// Entry: argv + stdio setup, then call the C `main`.
// ===========================================================================
#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    unsafe {
        stdin = addr_of_mut!(F_STDIN);
        stdout = addr_of_mut!(F_STDOUT);
        stderr = addr_of_mut!(F_STDERR);
    }
    let (argc, argv) = build_argv();
    let code = unsafe { main(argc, argv) };
    rt::sys_exit(code as i64 as u64);
}

/// Build a C `argv` from oxbow's whitespace-separated argument string. argv[0]
/// is a program-name placeholder (oxbow doesn't pass it); argv[1..] are the
/// tokens. Allocations leak — the whole address space is reclaimed on exit.
fn build_argv() -> (i32, *const *const u8) {
    let mut argv: Vec<*const u8> = Vec::new();
    argv.push(b"prog\0".as_ptr());
    for tok in rt::args() {
        let mut s: Vec<u8> = Vec::with_capacity(tok.len() + 1);
        s.extend_from_slice(tok);
        s.push(0);
        argv.push(s.as_ptr());
        core::mem::forget(s);
    }
    argv.push(core::ptr::null());
    let argc = (argv.len() - 1) as i32;
    let ptr = argv.as_ptr();
    core::mem::forget(argv);
    (argc, ptr)
}

unsafe fn out_fd(fd: i32, s: &[u8]) {
    if fd == 1 || fd == 2 {
        rt::stdout_write(s);
    }
}

// ===========================================================================
// <unistd.h> / <fcntl.h>: file descriptors over capabilities.
// fds 0/1/2 = the tty; 3.. index the fd table, each holding an fs file cap.
// ===========================================================================
#[derive(Clone, Copy)]
struct FdSlot {
    handle: Handle,
    off: u64,
    used: bool,
}
const MAX_FD: usize = 32;
static mut FDS: [FdSlot; MAX_FD] = [FdSlot { handle: 0, off: 0, used: false }; MAX_FD];

unsafe fn cstr_len(s: *const u8) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

unsafe fn fs_read(cap: Handle, off: u64, out: &mut [u8]) -> usize {
    let mut m = MsgBuf::new(TAG_FS_READ);
    m.data[0] = off;
    m.data_len = 1;
    if rt::sys_call(cap, &mut m).is_err() {
        return 0;
    }
    let count = (m.data[0] as usize).min(out.len()).min(56);
    core::ptr::copy_nonoverlapping((m.data.as_ptr() as *const u8).add(8), out.as_mut_ptr(), count);
    count
}

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
            -1
        }
        None => -1,
    }
}

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
pub unsafe extern "C" fn write(fd: i32, buf: *const u8, len: usize) -> isize {
    if buf.is_null() {
        return -1;
    }
    if fd == 1 || fd == 2 {
        out_fd(fd, core::slice::from_raw_parts(buf, len));
        len as isize
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: i32) -> i32 {
    if fd < 3 || fd as usize >= MAX_FD {
        return 0;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if slot.used {
        let _ = rt::sys_close(slot.handle);
        slot.used = false;
    }
    0
}

/// `lseek` — only SEEK_SET (0) on a read-only file fd (sets the read offset).
#[no_mangle]
pub unsafe extern "C" fn lseek(fd: i32, off: i64, whence: i32) -> i64 {
    if fd < 3 || fd as usize >= MAX_FD {
        return -1;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if !slot.used {
        return -1;
    }
    match whence {
        0 => slot.off = off as u64,           // SEEK_SET
        1 => slot.off = slot.off + off as u64, // SEEK_CUR
        _ => return -1,
    }
    slot.off as i64
}

// ===========================================================================
// <stdio.h>: FILE* is a thin wrapper over an fd. stdin/stdout/stderr are the tty.
// ===========================================================================
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FILE {
    fd: i32,
    eof: i32,
}
static mut F_STDIN: FILE = FILE { fd: 0, eof: 0 };
static mut F_STDOUT: FILE = FILE { fd: 1, eof: 0 };
static mut F_STDERR: FILE = FILE { fd: 2, eof: 0 };
#[no_mangle]
pub static mut stdin: *mut FILE = core::ptr::null_mut();
#[no_mangle]
pub static mut stdout: *mut FILE = core::ptr::null_mut();
#[no_mangle]
pub static mut stderr: *mut FILE = core::ptr::null_mut();

const MAX_FILES: usize = 16;
static mut FILES: [FILE; MAX_FILES] = [FILE { fd: -1, eof: 0 }; MAX_FILES];

#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const u8, _mode: *const u8) -> *mut FILE {
    let fd = open(path, 0);
    if fd < 0 {
        return core::ptr::null_mut();
    }
    for i in 0..MAX_FILES {
        let f = &mut (*addr_of_mut!(FILES))[i];
        if f.fd < 0 {
            f.fd = fd;
            f.eof = 0;
            return f as *mut FILE;
        }
    }
    close(fd);
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return -1;
    }
    let fd = (*stream).fd;
    if fd >= 3 {
        close(fd);
        (*stream).fd = -1; // return the FILE slot to the pool
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        return -1;
    }
    let mut b = [0u8; 1];
    if read((*stream).fd, b.as_mut_ptr(), 1) == 1 {
        b[0] as i32
    } else {
        (*stream).eof = 1;
        -1 // EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn fgets(buf: *mut u8, n: i32, stream: *mut FILE) -> *mut u8 {
    if buf.is_null() || n <= 0 || stream.is_null() {
        return core::ptr::null_mut();
    }
    let mut i = 0usize;
    let max = (n - 1) as usize;
    while i < max {
        let c = fgetc(stream);
        if c < 0 {
            break;
        }
        *buf.add(i) = c as u8;
        i += 1;
        if c == b'\n' as i32 {
            break;
        }
    }
    if i == 0 {
        return core::ptr::null_mut(); // EOF with nothing read
    }
    *buf.add(i) = 0;
    buf
}

#[no_mangle]
pub unsafe extern "C" fn fread(ptr: *mut u8, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 {
        return 0;
    }
    let total = size * nmemb;
    let mut done = 0usize;
    while done < total {
        let n = read((*stream).fd, ptr.add(done), total - done);
        if n <= 0 {
            break;
        }
        done += n as usize;
    }
    done / size
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(ptr: *const u8, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
    if ptr.is_null() || stream.is_null() || size == 0 {
        return 0;
    }
    let total = size * nmemb;
    out_fd((*stream).fd, core::slice::from_raw_parts(ptr, total));
    nmemb
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const u8, stream: *mut FILE) -> i32 {
    let n = cstr_len(s);
    let fd = if stream.is_null() { 1 } else { (*stream).fd };
    out_fd(fd, core::slice::from_raw_parts(s, n));
    0
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: i32, stream: *mut FILE) -> i32 {
    let fd = if stream.is_null() { 1 } else { (*stream).fd };
    out_fd(fd, &[c as u8]);
    c
}

#[no_mangle]
pub unsafe extern "C" fn putchar(c: i32) -> i32 {
    out_fd(1, &[c as u8]);
    c
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const u8) -> i32 {
    let n = cstr_len(s);
    out_fd(1, core::slice::from_raw_parts(s, n));
    out_fd(1, b"\n");
    0
}

#[no_mangle]
pub unsafe extern "C" fn feof(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        0
    } else {
        (*stream).eof
    }
}

#[no_mangle]
pub extern "C" fn fflush(_stream: *mut FILE) -> i32 {
    0 // unbuffered
}

// --- the printf family, sharing one formatter ---
unsafe fn print_uint(emit: &mut dyn FnMut(&[u8]), mut v: u64, base: u64) -> i32 {
    if v == 0 {
        emit(b"0");
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
    emit(&tmp[i..]);
    (tmp.len() - i) as i32
}

unsafe fn print_int(emit: &mut dyn FnMut(&[u8]), v: i64) -> i32 {
    if v < 0 {
        emit(b"-");
        1 + print_uint(emit, (v as i128).unsigned_abs() as u64, 10)
    } else {
        print_uint(emit, v as u64, 10)
    }
}

unsafe fn vfmt(emit: &mut dyn FnMut(&[u8]), fmt: *const u8, ap: &mut VaList) -> i32 {
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
            // skip width/precision/length digits + 'l' (we don't honor them)
            while matches!(*fmt.add(i), b'0'..=b'9' | b'.' | b'l' | b'-' | b'+' | b' ' | b'#') {
                i += 1;
            }
            let spec = *fmt.add(i);
            match spec {
                b'd' | b'i' => w += print_int(emit, ap.next_arg::<i32>() as i64),
                b'u' => w += print_uint(emit, ap.next_arg::<u32>() as u64, 10),
                b'x' | b'p' => w += print_uint(emit, ap.next_arg::<usize>() as u64, 16),
                b'c' => {
                    emit(&[ap.next_arg::<i32>() as u8]);
                    w += 1;
                }
                b's' => {
                    let p = ap.next_arg::<*const u8>();
                    let n = cstr_len(p);
                    if p.is_null() {
                        emit(b"(null)");
                        w += 6;
                    } else {
                        emit(core::slice::from_raw_parts(p, n));
                        w += n as i32;
                    }
                }
                b'%' => {
                    emit(b"%");
                    w += 1;
                }
                0 => break,
                _ => {
                    emit(&[b'%', spec]);
                    w += 2;
                }
            }
            i += 1;
        } else {
            emit(&[c]);
            w += 1;
            i += 1;
        }
    }
    w
}

#[no_mangle]
pub unsafe extern "C" fn printf(fmt: *const u8, mut args: ...) -> i32 {
    let mut emit = |s: &[u8]| out_fd(1, s);
    vfmt(&mut emit, fmt, &mut args)
}

#[no_mangle]
pub unsafe extern "C" fn fprintf(stream: *mut FILE, fmt: *const u8, mut args: ...) -> i32 {
    let fd = if stream.is_null() { 1 } else { (*stream).fd };
    let mut emit = |s: &[u8]| out_fd(fd, s);
    vfmt(&mut emit, fmt, &mut args)
}

// ===========================================================================
// <stdlib.h>: malloc/free over oxbow-rt's slab heap (size header for free).
// ===========================================================================
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

#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut u8, size: usize) -> *mut u8 {
    if ptr.is_null() {
        return malloc(size);
    }
    if size == 0 {
        free(ptr);
        return core::ptr::null_mut();
    }
    let old_total = *(ptr.sub(HDR) as *const usize);
    let old = old_total - HDR;
    let new = malloc(size);
    if !new.is_null() {
        core::ptr::copy_nonoverlapping(ptr, new, old.min(size));
        free(ptr);
    }
    new
}

#[no_mangle]
pub extern "C" fn exit(code: i32) -> ! {
    rt::sys_exit(code as i64 as u64);
}

#[no_mangle]
pub extern "C" fn abort() -> ! {
    rt::sys_exit(134);
}

#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    if s.is_null() {
        return 0;
    }
    let mut i = 0;
    while is_space_b(*s.add(i)) {
        i += 1;
    }
    let mut neg = false;
    match *s.add(i) {
        b'-' => {
            neg = true;
            i += 1;
        }
        b'+' => i += 1,
        _ => {}
    }
    let mut v: i64 = 0;
    while (*s.add(i)).is_ascii_digit() {
        v = v * 10 + (*s.add(i) - b'0') as i64;
        i += 1;
    }
    (if neg { -v } else { v }) as i32
}

#[no_mangle]
pub extern "C" fn abs(v: i32) -> i32 {
    v.unsigned_abs() as i32
}

// ===========================================================================
// <string.h> (memcpy/memset/memmove/memcmp come from compiler-builtins).
// ===========================================================================
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    cstr_len(s)
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0;
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

#[no_mangle]
pub unsafe extern "C" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (ca, cb) = (*a.add(i), *b.add(i));
        if ca != cb {
            return ca as i32 - cb as i32;
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strcpy(dst: *mut u8, src: *const u8) -> *mut u8 {
    let mut i = 0;
    loop {
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    while i < n {
        *dst.add(i) = 0;
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn strchr(s: *const u8, c: i32) -> *const u8 {
    let target = c as u8;
    let mut i = 0;
    loop {
        let ch = *s.add(i);
        if ch == target {
            return s.add(i);
        }
        if ch == 0 {
            return core::ptr::null();
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: usize) -> *const u8 {
    let target = c as u8;
    for i in 0..n {
        if *s.add(i) == target {
            return s.add(i);
        }
    }
    core::ptr::null()
}

// ===========================================================================
// <ctype.h>
// ===========================================================================
fn is_space_b(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
#[no_mangle]
pub extern "C" fn isspace(c: i32) -> i32 {
    is_space_b(c as u8) as i32
}
#[no_mangle]
pub extern "C" fn isdigit(c: i32) -> i32 {
    (c >= b'0' as i32 && c <= b'9' as i32) as i32
}
#[no_mangle]
pub extern "C" fn isalpha(c: i32) -> i32 {
    ((c as u8).is_ascii_alphabetic()) as i32
}
#[no_mangle]
pub extern "C" fn isalnum(c: i32) -> i32 {
    ((c as u8).is_ascii_alphanumeric()) as i32
}
#[no_mangle]
pub extern "C" fn isupper(c: i32) -> i32 {
    ((c as u8).is_ascii_uppercase()) as i32
}
#[no_mangle]
pub extern "C" fn islower(c: i32) -> i32 {
    ((c as u8).is_ascii_lowercase()) as i32
}
#[no_mangle]
pub extern "C" fn toupper(c: i32) -> i32 {
    (c as u8).to_ascii_uppercase() as i32
}
#[no_mangle]
pub extern "C" fn tolower(c: i32) -> i32 {
    (c as u8).to_ascii_lowercase() as i32
}
