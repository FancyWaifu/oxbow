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
use oxbow_abi::{Handle, MsgBuf, BOOT_EP, BOOT_NET_EP, BOOT_MEM, FS_FILE, TAG_FS_READ, TAG_FS_WRITE};
use oxbow_rt as rt;

#[cfg(feature = "entry")]
extern "C" {
    fn main(argc: i32, argv: *const *const u8) -> i32;
}

// ===========================================================================
// Entry: argv + stdio setup, then call the C `main`. Gated behind the default
// `entry` feature: spawned C programs (lua/tcc/curl/...) use it. A libc-hosted
// BOOT MODULE (fsd) has no argv page / tty stdout, so it links libc with
// `default-features = false` and supplies its own oxbow_main; malloc still works
// (rt's allocator lazily maps from BOOT_MEM, independent of this entry).
// ===========================================================================
#[cfg(feature = "entry")]
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
#[cfg(feature = "entry")]
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
    } else if fd >= 3 {
        fs_write_fd(fd, s);
    }
}

/// Write `s` to an open file fd (>=3) via the 48-byte FS_WRITE protocol, looping
/// at the fd's offset; the fs grows the file block by block. Returns bytes
/// written. This is the single file-output sink — `write`, `fwrite`, `fputc`,
/// `fputs`, and the printf family (via `out_fd`) all funnel through it.
unsafe fn fs_write_fd(fd: i32, s: &[u8]) -> usize {
    if fd < 3 || fd as usize >= MAX_FD {
        return 0;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if !slot.used {
        return 0;
    }
    let mut i = 0usize;
    while i < s.len() {
        let n = core::cmp::min(48, s.len() - i);
        let mut m = MsgBuf::new(TAG_FS_WRITE);
        m.data[0] = slot.off;
        m.data[1] = n as u64;
        let dst = m.data.as_mut_ptr() as *mut u8;
        core::ptr::copy_nonoverlapping(s[i..].as_ptr(), dst.add(16), n);
        m.data_len = 8;
        if rt::sys_call(slot.handle, &mut m).is_err() {
            break;
        }
        let wrote = m.data[0] as usize;
        if wrote == 0 {
            break; // file-size ceiling or arena exhausted
        }
        slot.off += wrote as u64;
        if slot.off > slot.size {
            slot.size = slot.off;
        }
        i += wrote;
    }
    i
}

// ===========================================================================
// <unistd.h> / <fcntl.h>: file descriptors over capabilities.
// fds 0/1/2 = the tty; 3.. index the fd table, each holding an fs file cap.
// ===========================================================================
#[derive(Clone, Copy)]
struct FdSlot {
    handle: Handle,
    off: u64,
    /// High-water mark of bytes in the file — tracks growth so `lseek(SEEK_END)`
    /// and `ftell` work on a file we are writing.
    size: u64,
    used: bool,
    /// True if this fd is a BSD socket (handle = a TCP socket cap, or 0 until
    /// `connect`). read/write/close then route through `rt::tcp` instead of the fs.
    is_sock: bool,
    /// True if this fd is one end of an AF_UNIX channel (handle = a Channel cap);
    /// read/write/close + sendmsg/recvmsg route through `rt::channel` (§40).
    is_chan: bool,
    /// O_NONBLOCK (fcntl F_SETFL): channel reads return EAGAIN instead of blocking.
    nonblock: bool,
    /// True if this fd is a memfd/shm region (handle = an Shm cap). mmap maps it
    /// RW into the AS; the fd (its cap) can be passed via SCM_RIGHTS for wl_shm.
    is_shm: bool,
}
const MAX_FD: usize = 32;
static mut FDS: [FdSlot; MAX_FD] = [FdSlot {
    handle: 0,
    off: 0,
    size: 0,
    used: false,
    is_sock: false,
    is_chan: false,
    nonblock: false,
    is_shm: false,
}; MAX_FD];

// <fcntl.h> open flags (must match libc/include/fcntl.h).
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0o100;
const O_TRUNC: i32 = 0o1000;

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
pub unsafe extern "C" fn open(path: *const u8, flags: i32) -> i32 {
    let n = cstr_len(path);
    if n == 0 {
        return -1;
    }
    let p = core::slice::from_raw_parts(path, n);
    // Resolve to a (file cap, initial size). O_CREAT/O_TRUNC route through the
    // fs CREATE op (create-or-truncate); a plain read goes through OPEN.
    let (cap, size) = if flags & (O_CREAT | O_TRUNC) != 0 {
        match rt::fs::create(BOOT_EP, p) {
            Some(c) => (c, 0u64),
            None => return -1,
        }
    } else {
        match rt::fs::open(BOOT_EP, p) {
            Some(node) if node.kind == FS_FILE => (node.cap, node.size as u64),
            Some(node) => {
                let _ = rt::sys_close(node.cap);
                return -1;
            }
            None => return -1,
        }
    };
    for i in 3..MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[i];
        if !slot.used {
            *slot = FdSlot {
                handle: cap,
                off: 0,
                size,
                used: true,
                is_sock: false,
                is_chan: false,
                nonblock: false,
                is_shm: false,
            };
            return i as i32;
        }
    }
    let _ = rt::sys_close(cap);
    -1
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
    if slot.is_chan {
        let out = core::slice::from_raw_parts_mut(buf, len);
        let mut caps: [Handle; 0] = [];
        match rt::channel::recv(slot.handle, out, &mut caps, slot.nonblock) {
            Some((n, _)) => return n as isize,
            None => {
                errno = 11; // EAGAIN (non-blocking, nothing ready)
                return -1;
            }
        }
    }
    if slot.is_sock {
        let out = core::slice::from_raw_parts_mut(buf, len);
        return rt::tcp::recv(slot.handle, out) as isize;
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
        return len as isize;
    }
    if fd >= 3 && (fd as usize) < MAX_FD {
        let slot = &(*addr_of_mut!(FDS))[fd as usize];
        if slot.used && slot.is_chan {
            let n = rt::channel::send(slot.handle, core::slice::from_raw_parts(buf, len), &[]);
            return if n == 0 && len > 0 { -1 } else { n as isize };
        }
        if slot.used && slot.is_sock {
            return sock_send(slot.handle, core::slice::from_raw_parts(buf, len));
        }
    }
    let w = fs_write_fd(fd, core::slice::from_raw_parts(buf, len));
    if w == 0 && len > 0 {
        -1
    } else {
        w as isize
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: i32) -> i32 {
    if fd >= EPOLL_FD_BASE {
        if let Some(inst) = epoll_inst(fd) {
            inst.used = false;
        }
        return 0;
    }
    if fd < 3 || fd as usize >= MAX_FD {
        return 0;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if slot.used {
        if slot.is_chan {
            rt::channel::close(slot.handle);
        } else if slot.is_shm {
            let _ = rt::sys_close(slot.handle); // close the cap; region freed on AS teardown
        } else if slot.is_sock {
            if slot.handle != 0 {
                rt::tcp::close(slot.handle);
            }
        } else {
            let _ = rt::sys_close(slot.handle);
        }
        slot.used = false;
        slot.is_sock = false;
        slot.is_chan = false;
        slot.nonblock = false;
        slot.is_shm = false;
    }
    0
}

// ===========================================================================
// BSD sockets (<sys/socket.h>, <netinet/in.h>, <netdb.h>) over the net server's
// TCP capability API. Enough for an HTTP client: socket/connect/send/recv/close,
// byte-order helpers, inet_pton, and getaddrinfo (numeric IPv4). IPv4/TCP only.
// ===========================================================================
#[no_mangle]
pub extern "C" fn htons(x: u16) -> u16 { x.to_be() }
#[no_mangle]
pub extern "C" fn ntohs(x: u16) -> u16 { u16::from_be(x) }
#[no_mangle]
pub extern "C" fn htonl(x: u32) -> u32 { x.to_be() }
#[no_mangle]
pub extern "C" fn ntohl(x: u32) -> u32 { u32::from_be(x) }

/// `struct sockaddr_in` — `sin_addr`/`sin_port` are network byte order, so the
/// in-memory bytes of `sin_addr` are the dotted-quad in order [a,b,c,d].
#[repr(C)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;

#[no_mangle]
pub unsafe extern "C" fn socket(domain: i32, ty: i32, _proto: i32) -> i32 {
    if domain != AF_INET || ty != SOCK_STREAM {
        return -1; // IPv4 TCP only
    }
    for i in 3..MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[i];
        if !slot.used {
            *slot = FdSlot {
                handle: 0,
                off: 0,
                size: 0,
                used: true,
                is_sock: true,
                is_chan: false,
                nonblock: false,
                is_shm: false,
            };
            return i as i32;
        }
    }
    -1
}

#[no_mangle]
pub unsafe extern "C" fn connect(fd: i32, addr: *const SockAddrIn, _len: u32) -> i32 {
    if fd < 3 || fd as usize >= MAX_FD || addr.is_null() {
        return -1;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if !slot.used || !slot.is_sock {
        return -1;
    }
    let ipb = (*addr).sin_addr.to_ne_bytes(); // memory bytes = [a,b,c,d]
    let port = u16::from_be((*addr).sin_port);
    match rt::tcp::connect(BOOT_NET_EP, ipb, port) {
        Some(s) => {
            slot.handle = s;
            0
        }
        None => -1,
    }
}

/// Loop `rt::tcp::send` (48-byte chunks) until all of `data` is sent.
unsafe fn sock_send(sock: Handle, data: &[u8]) -> isize {
    if sock == 0 {
        return -1;
    }
    let mut i = 0usize;
    while i < data.len() {
        let n = core::cmp::min(48, data.len() - i);
        if !rt::tcp::send(sock, &data[i..i + n]) {
            break;
        }
        i += n;
    }
    i as isize
}

/// Allocate a fresh fd backed by a channel capability `handle`.
unsafe fn alloc_chan_fd(handle: Handle) -> i32 {
    for i in 3..MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[i];
        if !slot.used {
            *slot = FdSlot {
                handle,
                off: 0,
                size: 0,
                used: true,
                is_sock: false,
                is_chan: true,
                nonblock: false,
                is_shm: false,
            };
            return i as i32;
        }
    }
    -1
}

/// `socketpair(AF_UNIX, SOCK_STREAM, 0, sv)` — a connected pair of channel fds
/// (§40). Either end streams bytes and can pass fds (their backing caps) via
/// sendmsg/recvmsg SCM_RIGHTS.
#[no_mangle]
pub unsafe extern "C" fn socketpair(domain: i32, ty: i32, _proto: i32, sv: *mut i32) -> i32 {
    if domain != 1 /*AF_UNIX*/ || (ty & 0xf) != 1 /*SOCK_STREAM*/ || sv.is_null() {
        return -1;
    }
    let Some((h0, h1)) = rt::channel::pair() else { return -1 };
    let fd0 = alloc_chan_fd(h0);
    let fd1 = alloc_chan_fd(h1);
    if fd0 < 0 || fd1 < 0 {
        return -1;
    }
    *sv = fd0;
    *sv.add(1) = fd1;
    0
}

// C struct mirrors for the ancillary-data path (must match <sys/socket.h>).
#[repr(C)]
struct CIovec {
    base: *mut u8,
    len: usize,
}
#[repr(C)]
struct CMsghdr {
    name: *mut u8,
    namelen: u32,
    iov: *mut CIovec,
    iovlen: usize,
    control: *mut u8,
    controllen: usize,
    flags: i32,
}
#[repr(C)]
struct CCmsghdr {
    cmsg_len: usize,
    cmsg_level: i32,
    cmsg_type: i32,
}
const CMSG_HDR_SZ: usize = 16; // aligned sizeof(struct cmsghdr)

/// `sendmsg` — gather the iov bytes and (for an SCM_RIGHTS control message) the
/// fds, then stream the bytes + each fd's backing capability over the channel.
#[no_mangle]
pub unsafe extern "C" fn sendmsg(fd: i32, msg: *const CMsghdr, _flags: i32) -> isize {
    if fd < 3 || fd as usize >= MAX_FD || msg.is_null() {
        return -1;
    }
    let slot = &(*addr_of_mut!(FDS))[fd as usize];
    if !slot.used || !slot.is_chan {
        return -1;
    }
    // Gather iov into a contiguous buffer.
    let mut data = [0u8; 4096];
    let mut dlen = 0usize;
    let iov = (*msg).iov;
    for i in 0..(*msg).iovlen {
        let v = &*iov.add(i);
        let n = core::cmp::min(v.len, data.len() - dlen);
        core::ptr::copy_nonoverlapping(v.base, data.as_mut_ptr().add(dlen), n);
        dlen += n;
    }
    // Collect fds from an SCM_RIGHTS control message -> their capability handles.
    let mut caps = [0u32; 8];
    let mut ncaps = 0usize;
    if !(*msg).control.is_null() && (*msg).controllen >= CMSG_HDR_SZ {
        let c = (*msg).control as *const CCmsghdr;
        if (*c).cmsg_level == 1 /*SOL_SOCKET*/ && (*c).cmsg_type == 1 /*SCM_RIGHTS*/ {
            let nfd = ((*c).cmsg_len - CMSG_HDR_SZ) / 4;
            let fds = ((*msg).control as *const u8).add(CMSG_HDR_SZ) as *const i32;
            for i in 0..nfd.min(caps.len()) {
                let pfd = *fds.add(i);
                if pfd >= 0 && (pfd as usize) < MAX_FD {
                    let ps = &(*addr_of_mut!(FDS))[pfd as usize];
                    if ps.used {
                        caps[ncaps] = ps.handle;
                        ncaps += 1;
                    }
                }
            }
        }
    }
    let n = rt::channel::send(slot.handle, &data[..dlen], &caps[..ncaps]);
    if n == 0 && dlen > 0 {
        -1
    } else {
        n as isize
    }
}

/// `recvmsg` — receive bytes into the iov and any passed capabilities, adopting
/// each as a fresh fd reported back as an SCM_RIGHTS control message.
#[no_mangle]
pub unsafe extern "C" fn recvmsg(fd: i32, msg: *mut CMsghdr, _flags: i32) -> isize {
    if fd < 3 || fd as usize >= MAX_FD || msg.is_null() {
        return -1;
    }
    let nonblock = {
        let slot = &(*addr_of_mut!(FDS))[fd as usize];
        if !slot.used || !slot.is_chan {
            return -1;
        }
        slot.nonblock
    };
    let handle = (*addr_of_mut!(FDS))[fd as usize].handle;
    let mut data = [0u8; 4096];
    let mut caps = [0u32; 8];
    let (n, nc) = match rt::channel::recv(handle, &mut data, &mut caps, nonblock) {
        Some(v) => v,
        None => {
            errno = 11; // EAGAIN
            return -1;
        }
    };
    // Scatter bytes into the iov.
    let mut copied = 0usize;
    let iov = (*msg).iov;
    for i in 0..(*msg).iovlen {
        if copied >= n {
            break;
        }
        let v = &*iov.add(i);
        let take = core::cmp::min(v.len, n - copied);
        core::ptr::copy_nonoverlapping(data.as_ptr().add(copied), v.base, take);
        copied += take;
    }
    // Report received caps as adopted fds in an SCM_RIGHTS control message.
    if nc > 0 && !(*msg).control.is_null() && (*msg).controllen >= CMSG_HDR_SZ + nc * 4 {
        let c = (*msg).control as *mut CCmsghdr;
        (*c).cmsg_len = CMSG_HDR_SZ + nc * 4;
        (*c).cmsg_level = 1; // SOL_SOCKET
        (*c).cmsg_type = 1; // SCM_RIGHTS
        let fds = ((*msg).control as *mut u8).add(CMSG_HDR_SZ) as *mut i32;
        for i in 0..nc {
            *fds.add(i) = alloc_cap_fd(caps[i]); // channel or shm, per its kind
        }
        (*msg).controllen = CMSG_HDR_SZ + nc * 4;
    } else {
        (*msg).controllen = 0;
    }
    copied as isize
}

#[no_mangle]
pub unsafe extern "C" fn send(fd: i32, buf: *const u8, len: usize, _flags: i32) -> isize {
    if fd < 3 || fd as usize >= MAX_FD || buf.is_null() {
        return -1;
    }
    let slot = &(*addr_of_mut!(FDS))[fd as usize];
    if !slot.used || !slot.is_sock {
        return -1;
    }
    sock_send(slot.handle, core::slice::from_raw_parts(buf, len))
}

#[no_mangle]
pub unsafe extern "C" fn recv(fd: i32, buf: *mut u8, len: usize, _flags: i32) -> isize {
    if fd < 3 || fd as usize >= MAX_FD || buf.is_null() {
        return -1;
    }
    let slot = &(*addr_of_mut!(FDS))[fd as usize];
    if !slot.used || !slot.is_sock {
        return -1;
    }
    let out = core::slice::from_raw_parts_mut(buf, len);
    rt::tcp::recv(slot.handle, out) as isize
}

#[no_mangle]
pub extern "C" fn shutdown(_fd: i32, _how: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn setsockopt(_fd: i32, _lvl: i32, _opt: i32, _val: *const u8, _len: u32) -> i32 {
    0
}
#[no_mangle]
pub unsafe extern "C" fn getsockopt(_fd: i32, _lvl: i32, _opt: i32, val: *mut u8, len: *mut u32) -> i32 {
    // Report no error (SO_ERROR=0) — curl checks this after connect() to confirm
    // the connection succeeded. Leaving the buffer untouched made it read garbage.
    if !val.is_null() && (len.is_null() || *len >= 4) {
        *(val as *mut i32) = 0;
    }
    0
}

/// Parse "a.b.c.d" into `dst` (network byte order: memory = [a,b,c,d]).
/// Returns 1 on success, 0 on a non-numeric string (matches inet_pton).
#[no_mangle]
pub unsafe extern "C" fn inet_pton(_af: i32, src: *const u8, dst: *mut u32) -> i32 {
    if src.is_null() || dst.is_null() {
        return 0;
    }
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    let mut val: u32 = 0;
    let mut seen = false;
    let mut i = 0usize;
    loop {
        let c = *src.add(i);
        if c == b'.' || c == 0 {
            if !seen || idx >= 4 || val > 255 {
                return 0;
            }
            octets[idx] = val as u8;
            idx += 1;
            val = 0;
            seen = false;
            if c == 0 {
                break;
            }
        } else if c.is_ascii_digit() {
            val = val * 10 + (c - b'0') as u32;
            seen = true;
        } else {
            return 0;
        }
        i += 1;
    }
    if idx != 4 {
        return 0;
    }
    *dst = u32::from_ne_bytes(octets);
    1
}
#[no_mangle]
pub unsafe extern "C" fn inet_addr(src: *const u8) -> u32 {
    let mut v: u32 = 0xffff_ffff;
    inet_pton(AF_INET, src, &mut v);
    v
}

// ===========================================================================
// epoll (userspace) — the readiness multiplexer libwayland's server event loop
// is built on. An epoll instance is a libc-side table of watched fds; epoll_wait
// polls each watched channel fd's readiness (SYS_CHANNEL_POLL) and reports the
// ready ones. epoll fds live in a high number range so they don't collide with
// the normal fd table. Busy-polls while waiting (no blocking-on-many-fds in the
// kernel yet) — fine for a demo compositor; a power-efficient version would block
// on a channel-wake. EPOLLIN=1 EPOLLOUT=4 EPOLLERR=8 EPOLLHUP=16.
const EPOLL_FD_BASE: i32 = 1000;
const EPOLL_MAX: usize = 4;
const EPOLL_WATCH: usize = 48;

#[repr(C, packed)]
struct EpollEvent {
    events: u32,
    data: u64,
}
#[derive(Clone, Copy)]
struct EpEntry {
    fd: i32,
    events: u32,
    data: u64,
}
#[derive(Clone, Copy)]
struct EpollInst {
    used: bool,
    n: usize,
    w: [EpEntry; EPOLL_WATCH],
}
static mut EPOLLS: [EpollInst; EPOLL_MAX] =
    [EpollInst { used: false, n: 0, w: [EpEntry { fd: 0, events: 0, data: 0 }; EPOLL_WATCH] };
        EPOLL_MAX];

#[no_mangle]
pub unsafe extern "C" fn epoll_create1(_flags: i32) -> i32 {
    for i in 0..EPOLL_MAX {
        let e = &mut (*addr_of_mut!(EPOLLS))[i];
        if !e.used {
            e.used = true;
            e.n = 0;
            return EPOLL_FD_BASE + i as i32;
        }
    }
    -1
}
#[no_mangle]
pub unsafe extern "C" fn epoll_create(_size: i32) -> i32 {
    epoll_create1(0)
}

unsafe fn epoll_inst(epfd: i32) -> Option<&'static mut EpollInst> {
    let idx = (epfd - EPOLL_FD_BASE) as usize;
    if epfd < EPOLL_FD_BASE || idx >= EPOLL_MAX {
        return None;
    }
    let e = &mut (*addr_of_mut!(EPOLLS))[idx];
    if e.used {
        Some(e)
    } else {
        None
    }
}

#[no_mangle]
pub unsafe extern "C" fn epoll_ctl(epfd: i32, op: i32, fd: i32, ev: *const EpollEvent) -> i32 {
    let Some(inst) = epoll_inst(epfd) else { return -1 };
    match op {
        1 => {
            // EPOLL_CTL_ADD
            if inst.n < EPOLL_WATCH && !ev.is_null() {
                inst.w[inst.n] = EpEntry { fd, events: (*ev).events, data: (*ev).data };
                inst.n += 1;
            } else {
                return -1;
            }
        }
        3 => {
            // EPOLL_CTL_MOD
            for i in 0..inst.n {
                if inst.w[i].fd == fd && !ev.is_null() {
                    inst.w[i].events = (*ev).events;
                    inst.w[i].data = (*ev).data;
                }
            }
        }
        2 => {
            // EPOLL_CTL_DEL
            let mut i = 0;
            while i < inst.n {
                if inst.w[i].fd == fd {
                    inst.w[i] = inst.w[inst.n - 1];
                    inst.n -= 1;
                } else {
                    i += 1;
                }
            }
        }
        _ => return -1,
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn epoll_wait(
    epfd: i32,
    events: *mut EpollEvent,
    maxevents: i32,
    timeout: i32,
) -> i32 {
    if epoll_inst(epfd).is_none() || events.is_null() || maxevents <= 0 {
        return -1;
    }
    let deadline = if timeout < 0 {
        u64::MAX
    } else {
        rt::sys_uptime_ms().wrapping_add(timeout as u64)
    };
    loop {
        let idx = (epfd - EPOLL_FD_BASE) as usize;
        let inst = &(*addr_of_mut!(EPOLLS))[idx];
        let mut count = 0usize;
        for i in 0..inst.n {
            if count >= maxevents as usize {
                break;
            }
            let w = inst.w[i];
            let mut revents = 0u32;
            if w.fd >= 3 && (w.fd as usize) < MAX_FD {
                let slot = &(*addr_of_mut!(FDS))[w.fd as usize];
                if slot.used && slot.is_chan {
                    let bits = rt::channel::poll(slot.handle);
                    if bits & 1 != 0 && w.events & 1 != 0 {
                        revents |= 1; // EPOLLIN
                    }
                    if bits & 2 != 0 {
                        revents |= 16; // EPOLLHUP
                    }
                    if bits & 4 != 0 && w.events & 4 != 0 {
                        revents |= 4; // EPOLLOUT
                    }
                }
            }
            if revents != 0 {
                let e = events.add(count);
                (*e).events = revents;
                (*e).data = w.data;
                count += 1;
            }
        }
        if count > 0 {
            return count as i32;
        }
        if timeout == 0 || rt::sys_uptime_ms() >= deadline {
            return 0;
        }
        // busy-poll (the timer preempts; a blocking version would await a wake)
    }
}

/// `struct addrinfo` — layout must match `<netdb.h>` (the C side uses our header).
#[repr(C)]
pub struct AddrInfo {
    pub ai_flags: i32,
    pub ai_family: i32,
    pub ai_socktype: i32,
    pub ai_protocol: i32,
    pub ai_addrlen: u32,
    pub ai_addr: *mut SockAddrIn,
    pub ai_canonname: *mut u8,
    pub ai_next: *mut AddrInfo,
}

/// Minimal getaddrinfo: resolves a numeric IPv4 `node` (DNS is a follow-up) and a
/// numeric/empty `service` port, returning one addrinfo. malloc'd; freeaddrinfo
/// releases it.
/// Resolve a hostname to an IPv4 address over DNS: bind a UDP socket, send a
/// standard A-record query to the DHCP-provided resolver (10.0.2.3 under QEMU
/// slirp), and parse the first A answer. Returns the address as a network-order
/// u32 (memory bytes [a,b,c,d]), or None.
// ---- c-ares DNS backend (cares_glue.c, compiled into libc by build.rs) -----
// getaddrinfo is backed by real c-ares system-wide. The C glue calls these
// extern "C" UDP helpers to reach the net server over the shared transfer frame.
// IP convention: a u32 packed `a<<24 | b<<16 | c<<8 | d` (to_be_bytes => wire).
extern "C" {
    /// Resolve `host` to an IPv4 address (4 wire-order bytes). Returns 1 on success.
    fn oxbow_cares_resolve(host: *const u8, out_ip: *mut u8) -> i32;
}

/// Attach (once) to the net server's shared UDP frame; null on failure.
#[no_mangle]
pub extern "C" fn ox_udp_attach() -> *mut u8 {
    rt::udp::attach(BOOT_NET_EP).unwrap_or(core::ptr::null_mut())
}

/// Bind a fresh UDP socket; returns its capability handle, or -1.
#[no_mangle]
pub extern "C" fn ox_udp_open() -> i64 {
    match rt::udp::bind(BOOT_NET_EP, 0) {
        Some((cap, _)) => cap as i64,
        None => -1,
    }
}

/// Send the first `len` bytes of the shared frame to `ip:port` on `cap`.
#[no_mangle]
pub extern "C" fn ox_udp_sendv(cap: u64, ip: u32, port: u16, len: usize) -> i32 {
    if rt::udp::sendv(cap as u32, ip.to_be_bytes(), port, len) {
        0
    } else {
        -1
    }
}

/// Non-blocking receive into the shared frame; returns datagram length (0=none).
#[no_mangle]
pub extern "C" fn ox_udp_recvv(cap: u64) -> i64 {
    rt::udp::recvv(cap as u32) as i64
}

/// Close a UDP socket capability (frees the net server's socket slot too).
#[no_mangle]
pub extern "C" fn ox_udp_close(cap: u64) {
    rt::udp::close(cap as u32);
}

/// Milliseconds since boot (the c-ares driving loop's deadline clock).
#[no_mangle]
pub extern "C" fn ox_uptime_ms() -> u64 {
    rt::sys_uptime_ms()
}

/// The DHCP-leased DNS resolver IP, packed `a<<24 | b<<16 | c<<8 | d`.
#[no_mangle]
pub extern "C" fn ox_dns_ip() -> u32 {
    u32::from_be_bytes(rt::udp::dns_server(BOOT_NET_EP))
}

/// Resolve `name` to a network-order IPv4 u32 via c-ares.
unsafe fn dns_resolve(name: *const u8) -> Option<u32> {
    let mut ip = [0u8; 4];
    if oxbow_cares_resolve(name, ip.as_mut_ptr()) == 1 {
        Some(u32::from_ne_bytes(ip)) // memory = [a,b,c,d] (network order)
    } else {
        None
    }
}

#[no_mangle]
pub unsafe extern "C" fn getaddrinfo(
    node: *const u8,
    service: *const u8,
    _hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
) -> i32 {
    if node.is_null() || res.is_null() {
        return -2; // EAI_NONAME-ish
    }
    let mut ip: u32 = 0;
    if inet_pton(AF_INET, node, &mut ip) != 1 {
        // Not a numeric IP — resolve the hostname over DNS.
        match dns_resolve(node) {
            Some(resolved) => ip = resolved,
            None => return -2, // EAI_NONAME
        }
    }
    let mut port: u16 = 0;
    if !service.is_null() {
        let mut i = 0usize;
        while *service.add(i) != 0 {
            let c = *service.add(i);
            if c.is_ascii_digit() {
                port = port.wrapping_mul(10).wrapping_add((c - b'0') as u16);
            }
            i += 1;
        }
    }
    let ai = malloc(core::mem::size_of::<AddrInfo>()) as *mut AddrInfo;
    let sa = malloc(core::mem::size_of::<SockAddrIn>()) as *mut SockAddrIn;
    if ai.is_null() || sa.is_null() {
        return -10;
    }
    (*sa).sin_family = AF_INET as u16;
    (*sa).sin_port = port.to_be();
    (*sa).sin_addr = ip;
    (*sa).sin_zero = [0; 8];
    (*ai).ai_flags = 0;
    (*ai).ai_family = AF_INET;
    (*ai).ai_socktype = SOCK_STREAM;
    (*ai).ai_protocol = 0;
    (*ai).ai_addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    (*ai).ai_addr = sa;
    (*ai).ai_canonname = core::ptr::null_mut();
    (*ai).ai_next = core::ptr::null_mut();
    *res = ai;
    0
}
#[no_mangle]
pub unsafe extern "C" fn freeaddrinfo(ai: *mut AddrInfo) {
    if !ai.is_null() {
        free((*ai).ai_addr as *mut u8);
        free(ai as *mut u8);
    }
}
#[no_mangle]
pub extern "C" fn gai_strerror(_e: i32) -> *const u8 {
    b"getaddrinfo error\0".as_ptr()
}

/// Degenerate `select`/`poll`: oxbow's socket recv is blocking, so report every
/// fd ready. The (easy-interface) caller then does a blocking recv. Returns the
/// number of fds (best-effort) so callers that check `> 0` proceed.
#[no_mangle]
pub extern "C" fn select(nfds: i32, _r: *mut u8, _w: *mut u8, _e: *mut u8, _t: *mut u8) -> i32 {
    if nfds > 0 {
        nfds
    } else {
        0
    }
}
#[no_mangle]
pub unsafe extern "C" fn poll(fds: *mut u8, nfds: u64, _timeout: i32) -> i32 {
    // struct pollfd { int fd; short events; short revents; }. Report only normal
    // readiness — POLLIN|POLLOUT — and NOTHING else: curl maps POLLPRI, POLLNVAL,
    // POLLERR, and POLLHUP in revents to its error condition (CURL_CSELECT_ERR),
    // which failed the connect even though the socket was fine.
    const READY: u16 = 0x001 | 0x004; // POLLIN | POLLOUT
    let mut ready = 0i32;
    for i in 0..nfds as usize {
        let p = fds.add(i * 8);
        let fd = *(p as *const i32);
        let events = *(p.add(4) as *const u16);
        let revents = if fd < 0 { 0 } else { events & READY };
        *(p.add(6) as *mut u16) = revents;
        if revents != 0 {
            ready += 1;
        }
    }
    ready
}

/// inet_ntop: format an IPv4 `src` (network-order u32) into "a.b.c.d".
#[no_mangle]
pub unsafe extern "C" fn inet_ntop(_af: i32, src: *const u32, dst: *mut u8, size: u32) -> *const u8 {
    if src.is_null() || dst.is_null() {
        return core::ptr::null();
    }
    let b = (*src).to_ne_bytes(); // [a,b,c,d]
    let mut n = 0usize;
    for (i, &oct) in b.iter().enumerate() {
        if i > 0 {
            if (n as u32) < size {
                *dst.add(n) = b'.';
            }
            n += 1;
        }
        let mut tmp = [0u8; 3];
        let mut k = 0;
        let mut v = oct;
        loop {
            tmp[k] = b'0' + v % 10;
            v /= 10;
            k += 1;
            if v == 0 {
                break;
            }
        }
        while k > 0 {
            k -= 1;
            if (n as u32) < size {
                *dst.add(n) = tmp[k];
            }
            n += 1;
        }
    }
    if (n as u32) < size {
        *dst.add(n) = 0;
    }
    dst
}

// Socket stubs not backed by the net server (curl binds locally / queries names).
#[no_mangle]
pub extern "C" fn bind(_fd: i32, _addr: *const u8, _len: u32) -> i32 {
    0
}
#[no_mangle]
pub unsafe extern "C" fn getsockname(_fd: i32, addr: *mut u8, len: *mut u32) -> i32 {
    if !addr.is_null() {
        core::ptr::write_bytes(addr, 0, 16);
        let sa = addr as *mut SockAddrIn;
        (*sa).sin_family = AF_INET as u16; // curl validates the local family
    }
    if !len.is_null() {
        *len = 16;
    }
    0
}
#[no_mangle]
pub unsafe extern "C" fn getpeername(fd: i32, addr: *mut u8, len: *mut u32) -> i32 {
    getsockname(fd, addr, len)
}
#[no_mangle]
pub extern "C" fn ioctl(_fd: i32, _req: u64, _arg: usize) -> i32 {
    0 // FIONBIO etc. — sockets stay blocking
}
#[no_mangle]
pub unsafe extern "C" fn fcntl(fd: i32, cmd: i32, arg: i64) -> i32 {
    // F_DUPFD(0)/F_DUPFD_CLOEXEC(1030): duplicate the fd into a new slot at or
    // above `arg`, sharing the same backing (channel handle etc.). libwayland's
    // event loop watches a DUP of each source fd, so this must work or epoll sees
    // a bogus fd. (No fd refcounting yet — a dup + close closes the channel; fine
    // for the current single-client flow.)
    if cmd == 0 || cmd == 1030 {
        if fd >= 3 && (fd as usize) < MAX_FD {
            let src = (*addr_of_mut!(FDS))[fd as usize];
            if src.used {
                let minfd = if arg >= 3 { arg as usize } else { 3 };
                for i in minfd..MAX_FD {
                    let slot = &mut (*addr_of_mut!(FDS))[i];
                    if !slot.used {
                        *slot = src;
                        return i as i32;
                    }
                }
            }
        }
        return -1;
    }
    // F_SETFL(4): honor O_NONBLOCK(04000) on channel fds (event loops need it).
    // F_GETFL(3): report the flag. Everything else is a no-op success.
    if fd >= 3 && (fd as usize) < MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
        if slot.used {
            if cmd == 4 {
                slot.nonblock = (arg & 0o4000) != 0;
            } else if cmd == 3 {
                return if slot.nonblock { 0o4000 } else { 0 };
            }
        }
    }
    0
}
#[no_mangle]
pub unsafe extern "C" fn fileno(stream: *mut FILE) -> i32 {
    if stream.is_null() {
        -1
    } else {
        (*stream).fd
    }
}
#[no_mangle]
pub extern "C" fn stat(_path: *const u8, _st: *mut u8) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn fstat(_fd: i32, _st: *mut u8) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn pipe(_fds: *mut i32) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn rename(_a: *const u8, _b: *const u8) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn geteuid() -> u32 {
    0
}
#[no_mangle]
pub unsafe extern "C" fn getpwuid_r(
    _u: u32,
    _p: *mut u8,
    _b: *mut u8,
    _n: usize,
    result: *mut *mut u8,
) -> i32 {
    if !result.is_null() {
        *result = core::ptr::null_mut();
    }
    1
}
#[no_mangle]
pub extern "C" fn getifaddrs(_a: *mut *mut u8) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn freeifaddrs(_a: *mut u8) {}

/// getentropy(2): fill `buf` with up to 256 bytes of CSPRNG output from the
/// kernel (ChaCha20, hardware-seeded). Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn getentropy(buf: *mut u8, len: usize) -> i32 {
    if len > 256 {
        return -1;
    }
    let slice = core::slice::from_raw_parts_mut(buf, len);
    if rt::sys_getentropy(slice).is_ok() {
        0
    } else {
        -1
    }
}

/// arc4random(3): a uniformly-random u32 straight from the kernel CSPRNG (real
/// entropy now — RDSEED/RDRAND-seeded ChaCha20 — not the old uptime xorshift).
#[no_mangle]
pub extern "C" fn arc4random() -> u32 {
    let mut b = [0u8; 4];
    let _ = rt::sys_getentropy(&mut b);
    u32::from_le_bytes(b)
}

/// arc4random_buf(3): fill `buf` with random bytes (any length; chunked to the
/// 256-byte getentropy limit).
#[no_mangle]
pub unsafe extern "C" fn arc4random_buf(buf: *mut u8, len: usize) {
    let mut off = 0;
    while off < len {
        let n = core::cmp::min(256, len - off);
        let _ = rt::sys_getentropy(core::slice::from_raw_parts_mut(buf.add(off), n));
        off += n;
    }
}

/// arc4random_uniform(3): a uniformly-random u32 in [0, bound), without modulo
/// bias (rejection sampling, as in OpenBSD).
#[no_mangle]
pub extern "C" fn arc4random_uniform(bound: u32) -> u32 {
    if bound < 2 {
        return 0;
    }
    let min = bound.wrapping_neg() % bound; // 2^32 mod bound
    loop {
        let r = arc4random();
        if r >= min {
            return r % bound;
        }
    }
}

/// rand(3): c-ares uses this only as a last-resort RNG fallback. Back it with the
/// kernel CSPRNG (ignoring the seed) — strictly better than a seeded LCG.
#[no_mangle]
pub extern "C" fn rand() -> i32 {
    (arc4random() & 0x7fff_ffff) as i32
}

/// srand(3): no-op — `rand` draws from the kernel CSPRNG, not a seeded sequence.
#[no_mangle]
pub extern "C" fn srand(_seed: u32) {}

/// recvfrom(2)/sendto(2): oxbow has no BSD fd sockets — net rides the capability
/// API. c-ares references these in its DEFAULT socket backend, which we replace
/// via ares_set_socket_functions, so they are never called; provide the symbols.
#[no_mangle]
pub extern "C" fn recvfrom(
    _fd: i32,
    _buf: *mut u8,
    _len: usize,
    _flags: i32,
    _addr: *mut u8,
    _addrlen: *mut u32,
) -> isize {
    unsafe { errno = 38 } // ENOSYS
    -1
}

#[no_mangle]
pub extern "C" fn sendto(
    _fd: i32,
    _buf: *const u8,
    _len: usize,
    _flags: i32,
    _addr: *const u8,
    _addrlen: u32,
) -> isize {
    unsafe { errno = 38 } // ENOSYS
    -1
}

/// getservbyname(3): service-name lookup. c-ares calls this only for a non-NULL
/// service argument (we pass NULL); stub to NULL so the symbol resolves.
#[no_mangle]
pub extern "C" fn getservbyname(_name: *const u8, _proto: *const u8) -> *mut u8 {
    core::ptr::null_mut()
}

// ---- libwayland OS shims ---------------------------------------------------
// `eventfd` is real (wl_display_create makes a terminate_efd added to its event
// loop): a plain fd slot whose counter never fires in our flow, so epoll (which
// only reports channel fds) simply never wakes on it. The rest are inert — they
// back wl_event_loop_add_signal/add_timer and wl_display_add_socket, paths a
// socketpair-driven compositor never takes; they only need to link.
#[no_mangle]
pub unsafe extern "C" fn eventfd(initval: u32, _flags: i32) -> i32 {
    for i in 3..MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[i];
        if !slot.used {
            *slot = FdSlot {
                handle: 0,
                off: initval as u64,
                size: 0,
                used: true,
                is_sock: false,
                is_chan: false,
                nonblock: false,
                is_shm: false,
            };
            return i as i32;
        }
    }
    -1
}
#[no_mangle]
pub unsafe extern "C" fn eventfd_read(fd: i32, value: *mut u64) -> i32 {
    if fd >= 3 && (fd as usize) < MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
        if slot.used {
            if !value.is_null() {
                *value = slot.off;
            }
            slot.off = 0;
            return 0;
        }
    }
    -1
}
#[no_mangle]
pub unsafe extern "C" fn eventfd_write(fd: i32, value: u64) -> i32 {
    if fd >= 3 && (fd as usize) < MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
        if slot.used {
            slot.off = slot.off.wrapping_add(value);
            return 0;
        }
    }
    -1
}

#[no_mangle]
pub extern "C" fn timerfd_create(_clockid: i32, _flags: i32) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn timerfd_settime(_fd: i32, _flags: i32, _new: *const u8, _old: *mut u8) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn timerfd_gettime(_fd: i32, _curr: *mut u8) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn signalfd(_fd: i32, _mask: *const u8, _flags: i32) -> i32 {
    -1
}
#[no_mangle]
pub extern "C" fn flock(_fd: i32, _op: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sigemptyset(_set: *mut u64) -> i32 {
    if !_set.is_null() {
        unsafe { *_set = 0 }
    }
    0
}
#[no_mangle]
pub extern "C" fn sigfillset(_set: *mut u64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sigaddset(_set: *mut u64, _signo: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sigdelset(_set: *mut u64, _signo: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sigismember(_set: *const u64, _signo: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn sigprocmask(_how: i32, _set: *const u64, _old: *mut u64) -> i32 {
    0
}
/// open_memstream(3): only reached by libwayland's WAYLAND_DEBUG message dump
/// (off by default). NULL is handled gracefully by the caller (skips the log).
#[no_mangle]
pub extern "C" fn open_memstream(_buf: *mut *mut u8, _len: *mut usize) -> *mut u8 {
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn strcspn(s: *const u8, reject: *const u8) -> usize {
    let mut n = 0usize;
    'outer: while *s.add(n) != 0 {
        let c = *s.add(n);
        let mut k = 0;
        while *reject.add(k) != 0 {
            if *reject.add(k) == c {
                break 'outer;
            }
            k += 1;
        }
        n += 1;
    }
    n
}
#[no_mangle]
pub unsafe extern "C" fn strtok_r(s: *mut u8, delim: *const u8, saveptr: *mut *mut u8) -> *mut u8 {
    let mut p = if s.is_null() { *saveptr } else { s };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    let is_delim = |c: u8| -> bool {
        let mut k = 0;
        while *delim.add(k) != 0 {
            if *delim.add(k) == c {
                return true;
            }
            k += 1;
        }
        false
    };
    while *p != 0 && is_delim(*p) {
        p = p.add(1);
    }
    if *p == 0 {
        *saveptr = p;
        return core::ptr::null_mut();
    }
    let tok = p;
    while *p != 0 && !is_delim(*p) {
        p = p.add(1);
    }
    if *p != 0 {
        *p = 0;
        *saveptr = p.add(1);
    } else {
        *saveptr = p;
    }
    tok
}
#[no_mangle]
pub unsafe extern "C" fn strerror_r(_e: i32, buf: *mut u8, len: usize) -> i32 {
    let msg = b"error";
    let n = core::cmp::min(msg.len(), len.saturating_sub(1));
    core::ptr::copy_nonoverlapping(msg.as_ptr(), buf, n);
    if len > 0 {
        *buf.add(n) = 0;
    }
    0
}
#[no_mangle]
pub unsafe extern "C" fn basename(path: *mut u8) -> *mut u8 {
    if path.is_null() {
        return path;
    }
    let mut last = path;
    let mut p = path;
    while *p != 0 {
        if *p == b'/' {
            last = p.add(1);
        }
        p = p.add(1);
    }
    last
}

/// `lseek` — SEEK_SET/CUR/END on a file fd. SEEK_END uses the tracked size
/// high-water mark, so tcc's "seek to end, ftell, seek back to 0 to patch the
/// ELF header" pattern works on a file we are writing.
#[no_mangle]
pub unsafe extern "C" fn lseek(fd: i32, off: i64, whence: i32) -> i64 {
    if fd < 3 || fd as usize >= MAX_FD {
        return -1;
    }
    let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
    if !slot.used {
        return -1;
    }
    let base = match whence {
        0 => 0i64,                // SEEK_SET
        1 => slot.off as i64,     // SEEK_CUR
        2 => slot.size as i64,    // SEEK_END
        _ => return -1,
    };
    let target = base + off;
    if target < 0 {
        return -1;
    }
    slot.off = target as u64;
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
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> *mut FILE {
    // Map the mode string to open flags. "r"=read, "w"=create+truncate+write,
    // "a"=append, and a '+' anywhere means read+write. ('b' is ignored — oxbow
    // makes no text/binary distinction.)
    let mut flags = 0i32;
    let mut append = false;
    if !mode.is_null() {
        match *mode {
            b'w' => flags = O_WRONLY | O_CREAT | O_TRUNC,
            b'a' => {
                flags = O_WRONLY | O_CREAT;
                append = true;
            }
            _ => flags = 0, // 'r' / default = read-only
        }
        let mut i = 1usize;
        while *mode.add(i) != 0 {
            if *mode.add(i) == b'+' {
                flags = (flags & !O_WRONLY) | O_RDWR;
            }
            i += 1;
        }
    }
    let fd = open(path, flags);
    if fd < 0 {
        return core::ptr::null_mut();
    }
    // Append mode: start the offset at end-of-file.
    if append {
        lseek(fd, 0, 2);
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

/// Format `x` in fixed notation with exactly `prec` fractional digits. Returns
/// the byte count. Integer part uses u64 (fine for an interpreter's range).
unsafe fn fmt_fixed(emit: &mut dyn FnMut(&[u8]), x: f64, prec: usize) -> i32 {
    let mut w = 0i32;
    // Round at the requested precision.
    let mut scale = 1.0f64;
    for _ in 0..prec {
        scale *= 10.0;
    }
    let rounded = floor(x * scale + 0.5) / scale;
    let ip = floor(rounded);
    w += print_uint(emit, ip as u64, 10);
    if prec > 0 {
        emit(b".");
        w += 1;
        let mut frac_scaled = (rounded - ip) * scale + 0.5;
        let mut fdv = frac_scaled as u64;
        // guard the carry case (e.g. 0.9995 @ prec 3): cap at scale-1
        let cap = scale as u64;
        if fdv >= cap {
            fdv = cap - 1;
        }
        let mut digits = [0u8; 24];
        let mut t = fdv;
        for k in (0..prec).rev() {
            digits[k] = b'0' + (t % 10) as u8;
            t /= 10;
        }
        emit(&digits[..prec]);
        w += prec as i32;
        let _ = &mut frac_scaled;
    }
    w
}

/// printf float conversions (f/e/g and a≈g). Pragmatic, not bit-exact, but good
/// enough for an interpreter: `%g` strips trailing zeros and falls back to
/// scientific outside [1e-4, 1e+P); Lua prints numbers via `%.14g`.
unsafe fn fmt_float(emit: &mut dyn FnMut(&[u8]), x: f64, prec: i32, spec: u8) -> i32 {
    let mut w = 0i32;
    // sign / non-finite
    let mut v = x;
    if v != v {
        emit(b"nan");
        return 3;
    }
    if v < 0.0 {
        emit(b"-");
        w += 1;
        v = -v;
    }
    if v == f64::INFINITY {
        emit(b"inf");
        return w + 3;
    }
    let lower = spec | 0x20; // fold case
    if lower == b'f' {
        let p = if prec < 0 { 6 } else { prec as usize };
        return w + fmt_fixed(emit, v, p);
    }
    // %e / %g share an exponent.
    let exp = if v == 0.0 { 0i32 } else { floor(log(v) / 2.302585092994046) as i32 };
    if lower == b'g' {
        // significant digits (default 6; 0 means 1).
        let mut sig = if prec < 0 { 6 } else { prec };
        if sig == 0 {
            sig = 1;
        }
        // Recompute exp accurately near powers of ten by checking the mantissa.
        let mut e = exp;
        let mut m = v / pow(10.0, e as f64);
        if m >= 10.0 {
            e += 1;
            m /= 10.0;
        }
        if v != 0.0 && m < 1.0 {
            e -= 1;
        }
        if e >= -4 && e < sig {
            // fixed notation with (sig-1-e) decimals, then strip trailing zeros.
            let dec = (sig - 1 - e).max(0) as usize;
            let mut tmp = TmpBuf::new();
            fmt_fixed(&mut |s| tmp.push(s), v, dec);
            let bytes = tmp.strip_g();
            emit(bytes);
            return w + bytes.len() as i32;
        } else {
            // scientific: mantissa with (sig-1) decimals, stripped, then eNN.
            let mant = v / pow(10.0, e as f64);
            let mut tmp = TmpBuf::new();
            fmt_fixed(&mut |s| tmp.push(s), mant, (sig - 1).max(0) as usize);
            let bytes = tmp.strip_g();
            emit(bytes);
            w += bytes.len() as i32;
            w += emit_exp(emit, e);
            return w;
        }
    }
    // %e
    let p = if prec < 0 { 6 } else { prec as usize };
    let mant = if v == 0.0 { 0.0 } else { v / pow(10.0, exp as f64) };
    w += fmt_fixed(emit, mant, p);
    w += emit_exp(emit, exp);
    w
}

/// Emit `e±NN` (at least two exponent digits), returning the byte count.
unsafe fn emit_exp(emit: &mut dyn FnMut(&[u8]), e: i32) -> i32 {
    emit(b"e");
    let (sign, mag) = if e < 0 { (b'-', -e) } else { (b'+', e) };
    emit(&[sign]);
    let mut buf = [0u8; 8];
    let mut n = 0;
    let mut m = mag;
    loop {
        buf[n] = b'0' + (m % 10) as u8;
        m /= 10;
        n += 1;
        if m == 0 {
            break;
        }
    }
    let mut out = 2i32;
    if n < 2 {
        emit(b"0");
        out += 1;
    }
    let mut k = n;
    while k > 0 {
        k -= 1;
        emit(&[buf[k]]);
        out += 1;
    }
    out
}

/// A small fixed scratch buffer so `%g` can post-process digits (strip zeros).
struct TmpBuf {
    buf: [u8; 64],
    len: usize,
}
impl TmpBuf {
    fn new() -> Self {
        TmpBuf { buf: [0; 64], len: 0 }
    }
    fn push(&mut self, s: &[u8]) {
        for &b in s {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
    }
    /// Strip trailing zeros (and a trailing '.') from a fixed-notation number.
    fn strip_g(&mut self) -> &[u8] {
        if self.buf[..self.len].contains(&b'.') {
            while self.len > 0 && self.buf[self.len - 1] == b'0' {
                self.len -= 1;
            }
            if self.len > 0 && self.buf[self.len - 1] == b'.' {
                self.len -= 1;
            }
        }
        &self.buf[..self.len]
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
            let mut prec: i32 = -1; // -1 = unspecified
            if *fmt.add(i) == b'.' {
                i += 1;
                if *fmt.add(i) == b'*' {
                    prec = ap.next_arg::<i32>();
                    i += 1;
                } else {
                    prec = 0;
                    while matches!(*fmt.add(i), b'0'..=b'9') {
                        prec = prec * 10 + (*fmt.add(i) - b'0') as i32;
                        i += 1;
                    }
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
                b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                    let v = ap.next_arg::<f64>();
                    w += fmt_float(emit, v, prec, spec);
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
// <string.h>
// ===========================================================================

// memcpy/memset/memmove/memcmp: compiler-builtins provides only WEAK versions,
// which a from-archive static link (tcc on oxbow) won't pull in — leaving the
// call site 0 and faulting. So define STRONG ones here. memcpy/memset use
// rep movsb/stosb so LLVM can't "optimize" the loop back into a memcpy/memset
// call (infinite recursion); the others use volatile loops for the same reason.
#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    core::arch::asm!(
        "rep movsb",
        inout("rcx") n => _,
        inout("rdi") dst => _,
        inout("rsi") src => _,
        options(nostack, preserves_flags),
    );
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    core::arch::asm!(
        "rep stosb",
        inout("rcx") n => _,
        inout("rdi") dst => _,
        in("al") c as u8,
        options(nostack, preserves_flags),
    );
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // Forward copy is safe when dst is at or below src, or the ranges don't
    // overlap; otherwise copy backward so we don't clobber unread source bytes.
    if (dst as usize) <= (src as usize) || (dst as usize) >= (src as usize) + n {
        memcpy(dst, src, n);
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        }
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = (core::ptr::read_volatile(a.add(i)), core::ptr::read_volatile(b.add(i)));
        if x != y {
            return x as i32 - y as i32;
        }
        i += 1;
    }
    0
}

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
pub extern "C" fn iscntrl(c: i32) -> i32 {
    ((c as u8).is_ascii_control()) as i32
}
#[no_mangle]
pub extern "C" fn isgraph(c: i32) -> i32 {
    ((c as u8).is_ascii_graphic()) as i32
}
#[no_mangle]
pub extern "C" fn ispunct(c: i32) -> i32 {
    ((c as u8).is_ascii_punctuation()) as i32
}
#[no_mangle]
pub extern "C" fn isxdigit(c: i32) -> i32 {
    ((c as u8).is_ascii_hexdigit()) as i32
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
pub unsafe extern "C" fn fseeko(stream: *mut FILE, off: i64, whence: i32) -> i32 {
    fseek(stream, off, whence)
}
#[no_mangle]
pub unsafe extern "C" fn ftello(stream: *mut FILE) -> i64 {
    ftell(stream)
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
/// Wrap an already-open fd in a FILE* (the fd's offset/size state lives in the
/// fd table, so the FILE just carries the fd). tcc uses this for its output.
#[no_mangle]
pub unsafe extern "C" fn fdopen(fd: i32, _mode: *const u8) -> *mut FILE {
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
/// Wall-clock baseline: oxbow has no RTC, so `time()` returns a fixed build-era
/// epoch plus uptime. Enough for TLS certificate validity windows (notBefore <
/// now < notAfter). Limitation: it doesn't track real time across reboots, so a
/// cert that expired shortly after this baseline can still validate; bump the
/// constant at build time to stay current. 2026-06-14 00:00:00 UTC.
const BUILD_EPOCH: i64 = 1_781_395_200;
#[no_mangle]
pub extern "C" fn time(t: *mut i64) -> i64 {
    let now = BUILD_EPOCH + rt::sys_uptime_ms() as i64 / 1000;
    if !t.is_null() {
        unsafe { *t = now };
    }
    now
}
#[no_mangle]
pub extern "C" fn clock() -> i64 {
    rt::sys_uptime_ms() as i64
}
#[repr(C)]
pub struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
#[no_mangle]
pub unsafe extern "C" fn clock_gettime(_clk: i32, ts: *mut Timespec) -> i32 {
    if !ts.is_null() {
        let ms = rt::sys_uptime_ms();
        (*ts).tv_sec = (ms / 1000) as i64;
        (*ts).tv_nsec = ((ms % 1000) * 1_000_000) as i64;
    }
    0
}
#[no_mangle]
pub unsafe extern "C" fn gmtime(t: *const i64) -> *mut i32 {
    localtime(t)
}
#[no_mangle]
pub extern "C" fn mktime(_tm: *const i32) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn lrint(x: f64) -> i64 {
    nearbyint(x) as i64
}
#[no_mangle]
pub extern "C" fn scalbn(x: f64, n: i32) -> f64 {
    ldexp(x, n)
}
#[no_mangle]
pub extern "C" fn difftime(a: i64, b: i64) -> f64 {
    (a - b) as f64
}
#[no_mangle]
pub unsafe extern "C" fn gmtime_r(t: *const i64, _res: *mut i32) -> *mut i32 {
    localtime(t)
}
#[no_mangle]
pub unsafe extern "C" fn localtime_r(t: *const i64, _res: *mut i32) -> *mut i32 {
    localtime(t)
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
pub extern "C" fn unlink(_p: *const u8) -> i32 {
    -1
}

// --- mmap/mprotect over oxbow: the JIT/exec primitive (§30). mmap hands out
//     anonymous RW pages from a reserved vaddr region; mprotect flips RW<->RX via
//     sys_protect (W^X-enforced in the kernel). PROT_* match the oxbow ABI. ---
use core::sync::atomic::{AtomicUsize, Ordering};
static MMAP_NEXT: AtomicUsize = AtomicUsize::new(0x5000_0000);

#[no_mangle]
pub unsafe extern "C" fn mmap(
    _addr: *mut u8,
    len: usize,
    prot: i32,
    _flags: i32,
    fd: i32,
    _off: i64,
) -> *mut u8 {
    let bytes = (len + 0xfff) & !0xfff;
    if bytes == 0 {
        return usize::MAX as *mut u8;
    }
    // File-backed mmap of a memfd/shm fd: map the SHARED region's frames, so the
    // mapper and anyone holding the same Shm cap (e.g. a wl_shm client and the
    // compositor) see the same memory.
    if fd >= 3 && (fd as usize) < MAX_FD {
        let slot = &(*addr_of_mut!(FDS))[fd as usize];
        if slot.used && slot.is_shm && slot.handle != 0 {
            let va = MMAP_NEXT.fetch_add(bytes, Ordering::Relaxed);
            match rt::sys_shm_map(slot.handle, va as u64) {
                Ok(_) => return va as *mut u8,
                Err(_) => return usize::MAX as *mut u8,
            }
        }
    }
    let va = MMAP_NEXT.fetch_add(bytes, Ordering::Relaxed);
    // Anonymous pages map RW first (W^X — can't map executable directly).
    if rt::sys_map(BOOT_MEM, va as u64, bytes as u64, 1 | 2).is_err() {
        return usize::MAX as *mut u8; // MAP_FAILED
    }
    if prot & 4 != 0 {
        let _ = rt::sys_protect(BOOT_MEM, va as u64, bytes as u64, 1 | 4); // -> RX
    }
    va as *mut u8
}

#[no_mangle]
pub unsafe extern "C" fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32 {
    let oxprot: u64 = if prot & 4 != 0 { 1 | 4 } else { 1 | 2 }; // RX or RW (W^X)
    let bytes = (len + 0xfff) & !0xfff;
    match rt::sys_protect(BOOT_MEM, addr as u64, bytes as u64, oxprot) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub extern "C" fn munmap(_a: *mut u8, _l: usize) -> i32 {
    0 // the whole AS is reclaimed on exit (§16); no per-region unmap yet
}

/// memfd_create(2): an anonymous shareable memory fd (Wayland's wl_shm pools).
/// The backing shm region is allocated by the subsequent ftruncate.
#[no_mangle]
pub unsafe extern "C" fn memfd_create(_name: *const u8, _flags: u32) -> i32 {
    for i in 3..MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[i];
        if !slot.used {
            *slot = FdSlot {
                handle: 0,
                off: 0,
                size: 0,
                used: true,
                is_sock: false,
                is_chan: false,
                nonblock: false,
                is_shm: true,
            };
            return i as i32;
        }
    }
    -1
}

/// ftruncate(2): on a memfd, allocate the shm region (ceil(length/page) frames).
/// Resizing an already-sized memfd is a no-op (wl_shm pool growth unsupported).
#[no_mangle]
pub unsafe extern "C" fn ftruncate(fd: i32, length: i64) -> i32 {
    if fd >= 3 && (fd as usize) < MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[fd as usize];
        if slot.used && slot.is_shm {
            if slot.handle == 0 && length > 0 {
                let pages = ((length as usize + 4095) / 4096) as u64;
                match rt::sys_shm_create(BOOT_MEM, pages) {
                    Ok(h) => {
                        slot.handle = h;
                        slot.size = length as u64;
                        return 0;
                    }
                    Err(_) => return -1,
                }
            }
            return 0;
        }
    }
    0
}

/// Adopt a capability received via SCM_RIGHTS as a fresh fd, reconstructing the
/// right flavor from its kind (channel stream vs shm region).
unsafe fn alloc_cap_fd(handle: Handle) -> i32 {
    let kind = rt::sys_cap_type(handle);
    for i in 3..MAX_FD {
        let slot = &mut (*addr_of_mut!(FDS))[i];
        if !slot.used {
            *slot = FdSlot {
                handle,
                off: 0,
                size: 0,
                used: true,
                is_sock: false,
                is_chan: kind == oxbow_abi::CAP_CHANNEL,
                nonblock: false,
                is_shm: kind == oxbow_abi::CAP_SHM,
            };
            return i as i32;
        }
    }
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
pub unsafe extern "C" fn frexp(x: f64, e: *mut i32) -> f64 {
    let set = |v: i32| if !e.is_null() { *e = v; };
    if x == 0.0 || x != x || fabs(x) == f64::INFINITY {
        set(0);
        return x;
    }
    let mut m = fabs(x);
    let mut exp = 0i32;
    while m >= 1.0 { m *= 0.5; exp += 1; }
    while m < 0.5 { m *= 2.0; exp -= 1; }
    set(exp);
    if x < 0.0 { -m } else { m }
}

/// Length of the initial span of `s` consisting only of bytes in `set`.
#[no_mangle]
pub unsafe extern "C" fn strspn(s: *const u8, set: *const u8) -> usize {
    let mut n = 0usize;
    'outer: while *s.add(n) != 0 {
        let c = *s.add(n);
        let mut k = 0usize;
        while *set.add(k) != 0 {
            if *set.add(k) == c {
                n += 1;
                continue 'outer;
            }
            k += 1;
        }
        break;
    }
    n
}

/// C-locale string collation is just byte comparison.
#[no_mangle]
pub unsafe extern "C" fn strcoll(a: *const u8, b: *const u8) -> i32 {
    strcmp(a, b)
}

// --- <math.h>: enough for Lua's number operators (// % ^) and stdio %g/%a. -----
// oxbow has hardware SSE doubles, so arithmetic is exact; these provide the named
// functions. floor/ceil/fmod/fabs/sqrt are exact; exp/log/pow are series-based
// (good to ~1e-12), sufficient for an interpreter's `^` operator and math lib.

#[no_mangle]
pub extern "C" fn fabs(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
}

#[no_mangle]
pub extern "C" fn floor(x: f64) -> f64 {
    // |x| >= 2^53 (or NaN/inf): already integral / pass through.
    if !(fabs(x) < 9.007199254740992e15) {
        return x;
    }
    let t = x as i64 as f64; // trunc toward zero
    if t > x { t - 1.0 } else { t }
}

#[no_mangle]
pub extern "C" fn ceil(x: f64) -> f64 {
    if !(fabs(x) < 9.007199254740992e15) {
        return x;
    }
    let t = x as i64 as f64;
    if t < x { t + 1.0 } else { t }
}

#[no_mangle]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    if y == 0.0 || !(fabs(x) < f64::INFINITY) {
        return f64::NAN;
    }
    let q = (x / y) as i64 as f64; // trunc(x/y)
    x - q * y
}

#[no_mangle]
pub extern "C" fn sqrt(x: f64) -> f64 {
    if x < 0.0 || x != x {
        return f64::NAN;
    }
    if x == 0.0 || x == f64::INFINITY {
        return x;
    }
    // Newton–Raphson: g <- (g + x/g)/2, to machine precision (stops when stable).
    let mut g = x;
    let mut prev = 0.0f64;
    let mut i = 0;
    while g != prev && i < 100 {
        prev = g;
        g = 0.5 * (g + x / g);
        i += 1;
    }
    g
}

#[no_mangle]
pub extern "C" fn exp(x: f64) -> f64 {
    if x != x {
        return x;
    }
    if x > 709.0 {
        return f64::INFINITY;
    }
    if x < -745.0 {
        return 0.0;
    }
    let k = x as i64; // trunc; r in (-1,1)
    let r = x - k as f64;
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    let mut n = 1.0f64;
    for _ in 0..18 {
        term *= r / n;
        sum += term;
        n += 1.0;
    }
    let e = 2.718281828459045f64;
    let mut ek = 1.0f64;
    if k >= 0 {
        for _ in 0..k { ek *= e; }
    } else {
        for _ in 0..(-k) { ek /= e; }
    }
    sum * ek
}

#[no_mangle]
pub extern "C" fn log(x: f64) -> f64 {
    if x < 0.0 || x != x {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    let mut m = x;
    let mut e = 0i32;
    while m >= 2.0 { m *= 0.5; e += 1; }
    while m < 1.0 { m *= 2.0; e -= 1; }
    // log(m), m in [1,2): 2*atanh((m-1)/(m+1)).
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = 0.0f64;
    let mut k = 0i32;
    for _ in 0..24 {
        sum += term / (2 * k + 1) as f64;
        term *= t2;
        k += 1;
    }
    2.0 * sum + (e as f64) * 0.6931471805599453
}

#[no_mangle]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    // Integer exponent: exact via repeated squaring (covers 2^10, x^2, …).
    if floor(y) == y && fabs(y) < 1024.0 {
        let mut n = y as i64;
        let neg = n < 0;
        if neg { n = -n; }
        let mut base = x;
        let mut acc = 1.0f64;
        while n > 0 {
            if n & 1 == 1 { acc *= base; }
            base *= base;
            n >>= 1;
        }
        return if neg { 1.0 / acc } else { acc };
    }
    if x <= 0.0 {
        return f64::NAN;
    }
    exp(y * log(x))
}

// --- Elementary transcendentals (libm). Series/range-reduction based, accurate
// to ~1e-12 — enough for an interpreter's math module (MicroPython, QuickJS). ---
const PI: f64 = 3.141592653589793;
const LN10: f64 = 2.302585092994046;
const LN2: f64 = 0.6931471805599453;

#[no_mangle]
pub extern "C" fn copysign(x: f64, y: f64) -> f64 {
    let a = fabs(x);
    if y < 0.0 || (y == 0.0 && 1.0 / y < 0.0) { -a } else { a }
}
#[no_mangle]
pub extern "C" fn trunc(x: f64) -> f64 {
    if !(fabs(x) < 9.007199254740992e15) { return x; }
    (x as i64) as f64
}
#[no_mangle]
pub extern "C" fn round(x: f64) -> f64 {
    if x < 0.0 { ceil(x - 0.5) } else { floor(x + 0.5) }
}
#[no_mangle]
pub unsafe extern "C" fn modf(x: f64, iptr: *mut f64) -> f64 {
    let ip = trunc(x);
    if !iptr.is_null() { *iptr = ip; }
    x - ip
}
#[no_mangle]
pub extern "C" fn log10(x: f64) -> f64 { log(x) / LN10 }
#[no_mangle]
pub extern "C" fn log2(x: f64) -> f64 { log(x) / LN2 }
#[no_mangle]
pub extern "C" fn expm1(x: f64) -> f64 { exp(x) - 1.0 }

/// sin via reduction to [-pi, pi] then Taylor (15 terms → ~1e-15 over the range).
#[no_mangle]
pub extern "C" fn sin(x: f64) -> f64 {
    if x != x || fabs(x) == f64::INFINITY { return f64::NAN; }
    let twopi = 2.0 * PI;
    let r = x - twopi * round(x / twopi); // r in [-pi, pi]
    let r2 = r * r;
    let mut term = r;
    let mut sum = r;
    let mut n = 1.0f64;
    for _ in 0..15 {
        term *= -r2 / ((2.0 * n) * (2.0 * n + 1.0));
        sum += term;
        n += 1.0;
    }
    sum
}
#[no_mangle]
pub extern "C" fn cos(x: f64) -> f64 {
    sin(x + PI / 2.0)
}
#[no_mangle]
pub extern "C" fn tan(x: f64) -> f64 {
    sin(x) / cos(x)
}

/// atan via |x|>1 reduction (pi/2 - atan(1/x)) and a half-range identity so the
/// final series argument stays small.
#[no_mangle]
pub extern "C" fn atan(x: f64) -> f64 {
    if x != x { return x; }
    if x < 0.0 { return -atan(-x); }
    if x > 1.0 { return PI / 2.0 - atan(1.0 / x); }
    // For x in (tan(pi/12), 1], reduce: atan(x) = pi/6 + atan((sqrt3 x -1)/(sqrt3 + x)).
    let sqrt3 = 1.7320508075688772;
    let mut add = 0.0;
    let mut v = x;
    if x > 0.2679491924311227 {
        add = PI / 6.0;
        v = (sqrt3 * x - 1.0) / (sqrt3 + x);
    }
    // series: v - v^3/3 + v^5/5 - ... (|v| <= tan(pi/12) ~ 0.27 → fast)
    let v2 = v * v;
    let mut term = v;
    let mut sum = v;
    let mut k = 1i32;
    for _ in 0..20 {
        term *= -v2;
        sum += term / (2 * k + 1) as f64;
        k += 1;
    }
    add + sum
}
#[no_mangle]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    if x > 0.0 {
        atan(y / x)
    } else if x < 0.0 {
        if y >= 0.0 { atan(y / x) + PI } else { atan(y / x) - PI }
    } else if y > 0.0 {
        PI / 2.0
    } else if y < 0.0 {
        -PI / 2.0
    } else {
        0.0
    }
}
#[no_mangle]
pub extern "C" fn asin(x: f64) -> f64 {
    if x < -1.0 || x > 1.0 { return f64::NAN; }
    if x == 1.0 { return PI / 2.0; }
    if x == -1.0 { return -PI / 2.0; }
    atan(x / sqrt(1.0 - x * x))
}
#[no_mangle]
pub extern "C" fn acos(x: f64) -> f64 {
    PI / 2.0 - asin(x)
}
#[no_mangle]
pub extern "C" fn sinh(x: f64) -> f64 {
    let e = exp(x);
    (e - 1.0 / e) * 0.5
}
#[no_mangle]
pub extern "C" fn cosh(x: f64) -> f64 {
    let e = exp(x);
    (e + 1.0 / e) * 0.5
}
#[no_mangle]
pub extern "C" fn tanh(x: f64) -> f64 {
    if x > 20.0 { return 1.0; }
    if x < -20.0 { return -1.0; }
    let e = exp(2.0 * x);
    (e - 1.0) / (e + 1.0)
}
#[no_mangle]
pub extern "C" fn asinh(x: f64) -> f64 {
    log(x + sqrt(x * x + 1.0))
}
#[no_mangle]
pub extern "C" fn acosh(x: f64) -> f64 {
    log(x + sqrt(x * x - 1.0))
}
#[no_mangle]
pub extern "C" fn atanh(x: f64) -> f64 {
    0.5 * log((1.0 + x) / (1.0 - x))
}
#[no_mangle]
pub extern "C" fn cbrt(x: f64) -> f64 {
    if x == 0.0 { return 0.0; }
    let s = if x < 0.0 { -1.0 } else { 1.0 };
    s * exp(log(fabs(x)) / 3.0)
}
#[no_mangle]
pub extern "C" fn hypot(x: f64, y: f64) -> f64 {
    sqrt(x * x + y * y)
}
/// Round to nearest integer (ties away from zero).
#[no_mangle]
pub extern "C" fn nearbyint(x: f64) -> f64 {
    if x >= 0.0 {
        floor(x + 0.5)
    } else {
        ceil(x - 0.5)
    }
}
#[no_mangle]
pub extern "C" fn rint(x: f64) -> f64 {
    nearbyint(x)
}

// --- <locale.h>: oxbow is "C" locale only (Lua reads the decimal point). -------
#[repr(C)]
pub struct Lconv {
    decimal_point: *const u8,
    thousands_sep: *const u8,
    grouping: *const u8,
}
// SAFETY: the pointers are to immortal 'static byte-string literals; read-only.
unsafe impl Sync for Lconv {}
static LCONV: Lconv = Lconv {
    decimal_point: b".\0".as_ptr(),
    thousands_sep: b"\0".as_ptr(),
    grouping: b"\0".as_ptr(),
};
#[no_mangle]
pub extern "C" fn localeconv() -> *const Lconv {
    &LCONV as *const Lconv
}
#[no_mangle]
pub extern "C" fn setlocale(_category: i32, _locale: *const u8) -> *const u8 {
    b"C\0".as_ptr()
}
#[no_mangle]
pub unsafe extern "C" fn strtof(s: *const u8, e: *mut *mut u8) -> f32 { strtod(s, e) as f32 }
#[no_mangle]
pub unsafe extern "C" fn strtold(s: *const u8, e: *mut *mut u8) -> f64 { strtod(s, e) }
