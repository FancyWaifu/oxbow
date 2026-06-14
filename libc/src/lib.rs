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
            // flags + width + precision (honored only enough to not misread args;
            // '*' consumes an int arg). Track the length modifier so %ld/%lx read
            // a 64-bit arg instead of 32 — tcc prints sizes/addresses this way.
            while matches!(*fmt.add(i), b'-' | b'+' | b' ' | b'#' | b'0') {
                i += 1;
            }
            while matches!(*fmt.add(i), b'0'..=b'9') {
                i += 1;
            }
            if *fmt.add(i) == b'*' {
                let _ = ap.next_arg::<i32>();
                i += 1;
            }
            if *fmt.add(i) == b'.' {
                i += 1;
                while matches!(*fmt.add(i), b'0'..=b'9') {
                    i += 1;
                }
                if *fmt.add(i) == b'*' {
                    let _ = ap.next_arg::<i32>();
                    i += 1;
                }
            }
            let mut lng = false;
            while matches!(*fmt.add(i), b'l' | b'z' | b'j' | b't') {
                lng = true;
                i += 1;
            }
            while *fmt.add(i) == b'h' {
                i += 1;
            }
            let spec = *fmt.add(i);
            match spec {
                b'd' | b'i' => {
                    let v = if lng { ap.next_arg::<i64>() } else { ap.next_arg::<i32>() as i64 };
                    w += print_int(emit, v);
                }
                b'u' => {
                    let v = if lng { ap.next_arg::<u64>() } else { ap.next_arg::<u32>() as u64 };
                    w += print_uint(emit, v, 10);
                }
                b'o' => {
                    let v = if lng { ap.next_arg::<u64>() } else { ap.next_arg::<u32>() as u64 };
                    w += print_uint(emit, v, 8);
                }
                b'x' | b'X' => {
                    let v = if lng { ap.next_arg::<u64>() } else { ap.next_arg::<u32>() as u64 };
                    w += print_uint(emit, v, 16);
                }
                b'p' => {
                    emit(b"0x");
                    w += 2 + print_uint(emit, ap.next_arg::<usize>() as u64, 16);
                }
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

// ===========================================================================
// Phase B: the rest of the surface TinyCC needs.
// ===========================================================================

// --- snprintf family (format into a buffer, sharing vfmt) ---
unsafe fn vsnprintf_impl(buf: *mut u8, size: usize, fmt: *const u8, ap: &mut VaList) -> i32 {
    let mut pos = 0usize;
    {
        let mut emit = |s: &[u8]| {
            for &b in s {
                if size > 0 && pos < size - 1 {
                    *buf.add(pos) = b;
                }
                pos += 1;
            }
        };
        let _ = vfmt(&mut emit, fmt, ap);
    }
    if size > 0 {
        *buf.add(pos.min(size - 1)) = 0;
    }
    pos as i32
}
#[no_mangle]
pub unsafe extern "C" fn vsnprintf(s: *mut u8, n: usize, fmt: *const u8, mut ap: VaList) -> i32 {
    vsnprintf_impl(s, n, fmt, &mut ap)
}
#[no_mangle]
pub unsafe extern "C" fn snprintf(s: *mut u8, n: usize, fmt: *const u8, mut args: ...) -> i32 {
    vsnprintf_impl(s, n, fmt, &mut args)
}
#[no_mangle]
pub unsafe extern "C" fn sprintf(s: *mut u8, fmt: *const u8, mut args: ...) -> i32 {
    vsnprintf_impl(s, usize::MAX, fmt, &mut args)
}
#[no_mangle]
pub unsafe extern "C" fn vfprintf(stream: *mut FILE, fmt: *const u8, mut ap: VaList) -> i32 {
    let fd = if stream.is_null() { 1 } else { (*stream).fd };
    let mut emit = |s: &[u8]| out_fd(fd, s);
    vfmt(&mut emit, fmt, &mut ap)
}

// --- <stdlib.h> string→number ---
unsafe fn parse_long(s: *const u8, endptr: *mut *mut u8, mut base: i32) -> i64 {
    let mut i = 0isize;
    while is_space_b(*s.offset(i)) {
        i += 1;
    }
    let mut neg = false;
    match *s.offset(i) {
        b'-' => { neg = true; i += 1; }
        b'+' => i += 1,
        _ => {}
    }
    if (base == 0 || base == 16) && *s.offset(i) == b'0' && (*s.offset(i + 1) | 0x20) == b'x' {
        base = 16;
        i += 2;
    } else if base == 0 && *s.offset(i) == b'0' {
        base = 8;
    } else if base == 0 {
        base = 10;
    }
    let mut v: i64 = 0;
    loop {
        let c = *s.offset(i);
        let d = match c {
            b'0'..=b'9' => (c - b'0') as i32,
            b'a'..=b'z' => (c - b'a' + 10) as i32,
            b'A'..=b'Z' => (c - b'A' + 10) as i32,
            _ => break,
        };
        if d >= base {
            break;
        }
        v = v * base as i64 + d as i64;
        i += 1;
    }
    if !endptr.is_null() {
        *endptr = s.offset(i) as *mut u8;
    }
    if neg { -v } else { v }
}
#[no_mangle]
pub unsafe extern "C" fn strtol(s: *const u8, e: *mut *mut u8, b: i32) -> i64 {
    parse_long(s, e, b)
}
#[no_mangle]
pub unsafe extern "C" fn strtoul(s: *const u8, e: *mut *mut u8, b: i32) -> u64 {
    parse_long(s, e, b) as u64
}
#[no_mangle]
pub unsafe extern "C" fn strtoll(s: *const u8, e: *mut *mut u8, b: i32) -> i64 {
    parse_long(s, e, b)
}
#[no_mangle]
pub unsafe extern "C" fn strtoull(s: *const u8, e: *mut *mut u8, b: i32) -> u64 {
    parse_long(s, e, b) as u64
}
#[no_mangle]
pub unsafe extern "C" fn strtod(s: *const u8, e: *mut *mut u8) -> f64 {
    // enough for tcc's float-constant parsing: integer part . fraction
    let mut i = 0isize;
    while is_space_b(*s.offset(i)) {
        i += 1;
    }
    let neg = *s.offset(i) == b'-';
    if neg || *s.offset(i) == b'+' {
        i += 1;
    }
    let mut v = 0f64;
    while (*s.offset(i)).is_ascii_digit() {
        v = v * 10.0 + (*s.offset(i) - b'0') as f64;
        i += 1;
    }
    if *s.offset(i) == b'.' {
        i += 1;
        let mut scale = 0.1f64;
        while (*s.offset(i)).is_ascii_digit() {
            v += (*s.offset(i) - b'0') as f64 * scale;
            scale *= 0.1;
            i += 1;
        }
    }
    if !e.is_null() {
        *e = s.offset(i) as *mut u8;
    }
    if neg { -v } else { v }
}

// --- qsort (insertion sort; small arrays, correctness over speed) ---
#[no_mangle]
pub unsafe extern "C" fn qsort(
    base: *mut u8,
    n: usize,
    size: usize,
    cmp: extern "C" fn(*const u8, *const u8) -> i32,
) {
    if n < 2 || size == 0 {
        return;
    }
    let tmp = malloc(size);
    if tmp.is_null() {
        return;
    }
    for i in 1..n {
        core::ptr::copy_nonoverlapping(base.add(i * size), tmp, size);
        let mut j = i;
        while j > 0 && cmp(base.add((j - 1) * size), tmp) > 0 {
            core::ptr::copy_nonoverlapping(base.add((j - 1) * size), base.add(j * size), size);
            j -= 1;
        }
        core::ptr::copy_nonoverlapping(tmp, base.add(j * size), size);
    }
    free(tmp);
}

// --- more <string.h> ---
#[no_mangle]
pub unsafe extern "C" fn strrchr(s: *const u8, c: i32) -> *const u8 {
    let t = c as u8;
    let mut last = core::ptr::null();
    let mut i = 0;
    loop {
        let ch = *s.add(i);
        if ch == t {
            last = s.add(i);
        }
        if ch == 0 {
            return last;
        }
        i += 1;
    }
}
#[no_mangle]
pub unsafe extern "C" fn strpbrk(s: *const u8, set: *const u8) -> *const u8 {
    let mut i = 0;
    while *s.add(i) != 0 {
        let mut j = 0;
        while *set.add(j) != 0 {
            if *s.add(i) == *set.add(j) {
                return s.add(i);
            }
            j += 1;
        }
        i += 1;
    }
    core::ptr::null()
}
#[no_mangle]
pub unsafe extern "C" fn strstr(h: *const u8, n: *const u8) -> *const u8 {
    let nl = cstr_len(n);
    if nl == 0 {
        return h;
    }
    let hl = cstr_len(h);
    if nl > hl {
        return core::ptr::null();
    }
    for i in 0..=(hl - nl) {
        if strncmp(h.add(i), n, nl) == 0 {
            return h.add(i);
        }
    }
    core::ptr::null()
}
#[no_mangle]
pub unsafe extern "C" fn strcat(dst: *mut u8, src: *const u8) -> *mut u8 {
    let d = cstr_len(dst);
    strcpy(dst.add(d), src);
    dst
}
#[no_mangle]
pub unsafe extern "C" fn strncat(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let d = cstr_len(dst);
    let mut i = 0;
    while i < n && *src.add(i) != 0 {
        *dst.add(d + i) = *src.add(i);
        i += 1;
    }
    *dst.add(d + i) = 0;
    dst
}
#[no_mangle]
pub unsafe extern "C" fn strdup(s: *const u8) -> *mut u8 {
    let n = cstr_len(s);
    let p = malloc(n + 1);
    if !p.is_null() {
        core::ptr::copy_nonoverlapping(s, p, n + 1);
    }
    p
}
static mut STRTOK_SAVE: *mut u8 = core::ptr::null_mut();
#[no_mangle]
pub unsafe extern "C" fn strtok(s: *mut u8, delim: *const u8) -> *mut u8 {
    let mut p = if s.is_null() { STRTOK_SAVE } else { s };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    while *p != 0 && !strchr(delim, *p as i32).is_null() {
        p = p.add(1);
    }
    if *p == 0 {
        STRTOK_SAVE = core::ptr::null_mut();
        return core::ptr::null_mut();
    }
    let start = p;
    while *p != 0 && strchr(delim, *p as i32).is_null() {
        p = p.add(1);
    }
    if *p != 0 {
        *p = 0;
        STRTOK_SAVE = p.add(1);
    } else {
        STRTOK_SAVE = core::ptr::null_mut();
    }
    start
}
#[no_mangle]
pub unsafe extern "C" fn strerror(_n: i32) -> *const u8 {
    b"error\0".as_ptr()
}

// --- more <stdio.h> ---
#[no_mangle]
pub unsafe extern "C" fn fseek(stream: *mut FILE, off: i64, whence: i32) -> i32 {
    if stream.is_null() {
        return -1;
    }
    if lseek((*stream).fd, off, whence) < 0 {
        -1
    } else {
        (*stream).eof = 0;
        0
    }
}
#[no_mangle]
pub unsafe extern "C" fn ftell(stream: *mut FILE) -> i64 {
    if stream.is_null() {
        return -1;
    }
    lseek((*stream).fd, 0, 1) // SEEK_CUR
}
#[no_mangle]
pub unsafe extern "C" fn getc(stream: *mut FILE) -> i32 {
    fgetc(stream)
}
#[no_mangle]
pub unsafe extern "C" fn putc(c: i32, stream: *mut FILE) -> i32 {
    fputc(c, stream)
}
#[no_mangle]
pub extern "C" fn ungetc(_c: i32, _stream: *mut FILE) -> i32 {
    -1 // unsupported (tcc rarely needs it)
}
#[no_mangle]
pub unsafe extern "C" fn perror(s: *const u8) {
    if !s.is_null() && cstr_len(s) > 0 {
        out_fd(2, core::slice::from_raw_parts(s, cstr_len(s)));
        out_fd(2, b": ");
    }
    out_fd(2, b"error\n");
}
#[no_mangle]
pub extern "C" fn ferror(_stream: *mut FILE) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn fdopen(_fd: i32, _mode: *const u8) -> *mut FILE {
    core::ptr::null_mut()
}

// --- globals C expects ---
#[no_mangle]
pub static mut errno: i32 = 0;
#[no_mangle]
pub static mut environ: *const *const u8 = core::ptr::null();

// --- stubs (features that won't run on oxbow but must link) ---
#[no_mangle]
pub extern "C" fn getenv(_name: *const u8) -> *const u8 {
    core::ptr::null()
}
#[no_mangle]
pub unsafe extern "C" fn realpath(path: *const u8, resolved: *mut u8) -> *mut u8 {
    if resolved.is_null() {
        return strdup(path);
    }
    strcpy(resolved, path)
}
#[no_mangle]
pub extern "C" fn sem_init(_s: *mut i32, _p: i32, _v: u32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sem_post(_s: *mut i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sem_wait(_s: *mut i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sem_destroy(_s: *mut i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn time(_t: *mut i64) -> i64 {
    rt::sys_uptime_ms() as i64 / 1000
}
#[no_mangle]
pub extern "C" fn clock() -> i64 {
    rt::sys_uptime_ms() as i64
}
#[no_mangle]
pub extern "C" fn gettimeofday(tv: *mut i64, _tz: *mut u8) -> i32 {
    if !tv.is_null() {
        let ms = rt::sys_uptime_ms();
        unsafe {
            *tv = (ms / 1000) as i64;
            *tv.add(1) = ((ms % 1000) * 1000) as i64;
        }
    }
    0
}
#[no_mangle]
pub extern "C" fn signal(_n: i32, _h: usize) -> usize {
    0
}
#[no_mangle]
pub extern "C" fn raise(_n: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn dlopen(_p: *const u8, _f: i32) -> *const u8 {
    core::ptr::null()
}
#[no_mangle]
pub extern "C" fn dlsym(_h: *const u8, _s: *const u8) -> *const u8 {
    core::ptr::null()
}
#[no_mangle]
pub extern "C" fn dlclose(_h: *const u8) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn dlerror() -> *const u8 {
    core::ptr::null()
}
#[no_mangle]
pub extern "C" fn unlink(_p: *const u8) -> i32 {
    -1
}

// --- mmap/mprotect: STUBS for now (Phase C makes them real over a capability).
//     tcc -run needs these; linking needs them defined. ---
#[no_mangle]
pub extern "C" fn mmap(_a: *mut u8, _l: usize, _p: i32, _f: i32, _fd: i32, _o: i64) -> *mut u8 {
    usize::MAX as *mut u8 // MAP_FAILED
}
#[no_mangle]
pub extern "C" fn munmap(_a: *mut u8, _l: usize) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn mprotect(_a: *mut u8, _l: usize, _p: i32) -> i32 {
    -1
}

// --- setjmp/longjmp (x86_64): save callee-saved + rsp + return address ---
core::arch::global_asm!(
    r#"
.global setjmp
setjmp:
    mov [rdi],    rbx
    mov [rdi+8],  rbp
    mov [rdi+16], r12
    mov [rdi+24], r13
    mov [rdi+32], r14
    mov [rdi+40], r15
    lea rax, [rsp+8]
    mov [rdi+48], rax
    mov rax, [rsp]
    mov [rdi+56], rax
    xor eax, eax
    ret
.global longjmp
longjmp:
    mov rbx, [rdi]
    mov rbp, [rdi+8]
    mov r12, [rdi+16]
    mov r13, [rdi+24]
    mov r14, [rdi+32]
    mov r15, [rdi+40]
    mov rsp, [rdi+48]
    mov eax, esi
    test eax, eax
    jnz 1f
    mov eax, 1
1:
    jmp [rdi+56]
"#
);

// --- last few for tcc (mostly stubs; long double approximated as double) ---
#[no_mangle]
pub extern "C" fn execvp(_p: *const u8, _a: *const *const u8) -> i32 { -1 }
#[no_mangle]
pub extern "C" fn freopen(_p: *const u8, _m: *const u8, _s: *mut FILE) -> *mut FILE {
    core::ptr::null_mut()
}
#[no_mangle]
pub extern "C" fn remove(_p: *const u8) -> i32 { -1 }
#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut u8, size: usize) -> *mut u8 {
    if buf.is_null() || size < 2 {
        return core::ptr::null_mut();
    }
    *buf = b'/';
    *buf.add(1) = 0;
    buf
}
static mut TM_STUB: [i32; 9] = [0; 9];
#[no_mangle]
pub unsafe extern "C" fn localtime(_t: *const i64) -> *mut i32 {
    addr_of_mut!(TM_STUB) as *mut i32
}
#[no_mangle]
pub extern "C" fn ldexp(x: f64, e: i32) -> f64 {
    let mut r = x;
    let mut n = e;
    while n > 0 { r *= 2.0; n -= 1; }
    while n < 0 { r *= 0.5; n += 1; }
    r
}
#[no_mangle]
pub extern "C" fn ldexpl(x: f64, e: i32) -> f64 { ldexp(x, e) }
#[no_mangle]
pub unsafe extern "C" fn strtof(s: *const u8, e: *mut *mut u8) -> f32 { strtod(s, e) as f32 }
#[no_mangle]
pub unsafe extern "C" fn strtold(s: *const u8, e: *mut *mut u8) -> f64 { strtod(s, e) }
