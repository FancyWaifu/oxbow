//! fsd — the lwext4/ext2 filesystem server (Stage 3: the real fs).
//!
//! Serves the oxbow FS IPC protocol (TAG_FS_*) on the filesystem endpoint, but
//! backed by a real ext2 filesystem on the virtio-blk disk via lwext4 — not the
//! old in-memory ramfs. The capability model is preserved: a client holds a
//! BADGED endpoint where the badge is a node id, and there is no global namespace
//! (L3). The server keeps an id->path table: each badge indexes a stored path,
//! which it rebuilds and hands to lwext4 (a path-based API). '..' is rejected, so
//! a dir cap still cannot escape its subtree.
//!
//! On first boot the disk is formatted and seeded from the USTAR initrd (the FHS
//! skeleton + the oxbow source tree); later boots mount the existing ext2 and the
//! whole tree — including everything written since — is already there.
//!
//! libc-hosted (for lwext4's malloc/memcpy/...) with its spawned-program entry
//! disabled; fsd supplies its own oxbow_main and logs to the console.
#![no_std]
#![no_main]

extern crate oxbow_libc as _;

use core::ffi::{c_int, c_void};
use oxbow_abi::{
    Handle, MsgBuf, BLK_CHUNK, BLK_XFER_SECTORS, BOOT_BLK_EP, BOOT_CONSOLE, BOOT_EP, BOOT_MEM,
    FS_DIR, FS_FILE, FS_INITRD, FS_O_CREATE, FS_O_EXCL, FS_O_TRUNC, FS_RIGHT_APPEND, FS_RIGHT_LIST,
    FS_RIGHT_NODELETE, FS_RIGHT_RW, FS_ROOT, FS_SYMLINK,
    MSG_DATA_WORDS, PROT_READ, PROT_WRITE, R_GRANT, R_SEND, TAG_BLK_ATTACH,
    TAG_BLK_FLUSH, TAG_BLK_READ, TAG_BLK_READN, TAG_BLK_WRITE, TAG_BLK_WRITEN, TAG_FS_CREATE,
    TAG_FS_LINK, TAG_FS_MKDIR, TAG_FS_NAMESPACE, TAG_FS_NS_MOUNT, TAG_FS_OPEN, TAG_FS_READ, TAG_FS_READ_BULK, TAG_FS_READDIR, TAG_FS_READLINK,
    TAG_FS_RELEASE, TAG_FS_RENAME, TAG_FS_SETTIMES, TAG_FS_SYMLINK, TAG_FS_SYNC, TAG_FS_TRUNCATE,
    TAG_FS_UNLINK, TAG_FS_WRITE,
};
use oxbow_rt as rt;

const SECTOR: usize = 512;
const READ_CHUNK: usize = 504; // §99: 504 B/IPC (MSG_DATA_WORDS=64 -> 512 B data, minus the count word)
/// Where fsd maps the shared block-transfer frame (above the rt heap window).
const FSD_XFER: usize = 0x3F00_0000;
static mut SHARED_OK: bool = false;

// --- §94: a one-block READ CACHE --------------------------------------------
// FS_READ serves 56 bytes per IPC, but `oxfs_pread` is PATH-based: each call
// re-resolves the path through lwext4 AND re-reads the 4 KiB ext2 block holding
// the offset. Loading a 100 KiB program (~1800 reads) or linking the 1.8 MiB
// libc (~32000 reads) therefore re-reads each block ~73 times — minutes for a
// compile. Cache the LAST 4 KiB block (keyed by path + block index): consecutive
// 56-byte reads of the same block then cost ONE path-resolved disk read, the rest
// are memcpys. Invalidated on any write so it never serves stale data.
const CBLK: usize = 4096;
const CWAYS: usize = 64; // 64 * 4 KiB = 256 KiB of cached blocks for ONE file
static mut C_PATH: [u8; 256] = [0; 256];
static mut C_PLEN: usize = 0;
static mut C_BLK: [u64; CWAYS] = [u64::MAX; CWAYS]; // block idx per way (MAX = empty)
static mut C_LEN: [usize; CWAYS] = [0; CWAYS]; // valid bytes per way
static mut C_NEXT: usize = 0; // round-robin eviction cursor
static mut C_BUF: [[u8; CBLK]; CWAYS] = [[0; CBLK]; CWAYS];
// §perf bulk read: fsd reads up to one page into here, then SYS_REPLY_BULK copies it
// straight into the caller's buffer (one round trip per page vs ~8 inline 504-B reads).
// +512 slack: cached_read writes in <=504-B chunks and may overshoot the page tail.
static mut BULKBUF: [u8; 4096 + 512] = [0; 4096 + 512];

fn cache_invalidate() {
    unsafe {
        C_BLK = [u64::MAX; CWAYS];
        C_PLEN = 0;
        oxfs_read_close(); // §perf: drop the held read handle too (it may now be stale)
    }
}

/// Serve up to `READ_CHUNK` bytes at `off` of the file at NUL-terminated path
/// `full` from an N-way block cache holding ONE file's 4 KiB blocks. Reads of a
/// single file (loading a program, tcc linking an archive) then re-read each block
/// from disk at most once instead of ~73 times; switching files flushes. `dst`
/// receives the bytes; returns the count.
unsafe fn cached_read(full: &[u8], off: u64, dst: *mut u8) -> usize {
    let blk = off / CBLK as u64;
    let plen = full.iter().position(|&b| b == 0).unwrap_or(full.len());
    let cp = core::ptr::addr_of!(C_PATH) as *const u8;
    // A different file than the cache currently holds → flush and adopt it.
    if !(C_PLEN == plen && core::slice::from_raw_parts(cp, plen) == &full[..plen]) {
        C_BLK = [u64::MAX; CWAYS];
        C_PLEN = plen;
        core::ptr::copy_nonoverlapping(full.as_ptr(), core::ptr::addr_of_mut!(C_PATH) as *mut u8, plen);
    }
    // Find the block among the ways; on a miss, read it into the next slot.
    let way = match (0..CWAYS).find(|&i| C_BLK[i] == blk) {
        Some(i) => i,
        None => {
            let i = C_NEXT;
            C_NEXT = (C_NEXT + 1) % CWAYS;
            let mut rd = 0usize;
            oxfs_pread2(
                full.as_ptr(),
                blk * CBLK as u64,
                core::ptr::addr_of_mut!(C_BUF[i]) as *mut c_void,
                CBLK,
                &mut rd,
            );
            C_BLK[i] = blk;
            C_LEN[i] = rd;
            i
        }
    };
    let within = (off % CBLK as u64) as usize;
    if within >= C_LEN[way] {
        return 0;
    }
    let n = core::cmp::min(C_LEN[way] - within, READ_CHUNK);
    core::ptr::copy_nonoverlapping((core::ptr::addr_of!(C_BUF[way]) as *const u8).add(within), dst, n);
    n
}

// --- §94: a write-COALESCING buffer ----------------------------------------
// TAG_FS_WRITE delivers <=48 bytes per message and oxfs_pwrite is path-based
// (ext4_fopen+fseek+fwrite+fclose EVERY call, plus a disk flush). Writing a
// multi-MiB file (a saved document, or tcc's output binary) one 48-byte open+
// flush at a time is hopeless. Buffer sequential writes into one 4 KiB block and
// run oxfs_pwrite ONCE per block (a ~85x cut in opens+flushes). Flushed on a
// block/file change and before any read/open/sync so readers never see stale data.
static mut W_PATH: [u8; 256] = [0; 256];
static mut W_PLEN: usize = 0;
static mut W_BLK: u64 = u64::MAX;
static mut W_LEN: usize = 0; // valid bytes in W_BUF
static mut W_DIRTY: bool = false;
static mut W_BUF: [u8; CBLK] = [0; CBLK];

unsafe fn wbuf_flush() {
    if W_DIRTY && W_PLEN > 0 && W_LEN > 0 {
        let mut wr = 0usize;
        oxfs_pwrite(
            core::ptr::addr_of!(W_PATH) as *const u8,
            W_BLK * CBLK as u64,
            core::ptr::addr_of!(W_BUF) as *const c_void,
            W_LEN,
            &mut wr,
        );
        oxfs_flush();
        let _ = wr;
        // The flush changed the on-disk bytes of W_PATH, so any block the read cache holds
        // for that file is now stale. Invalidate it — otherwise a read right after a write
        // (write -> wbuf -> flush-on-next-open -> read) can serve a stale block, e.g. the
        // empty block from when the file was just created, yielding a 0-byte read.
        cache_invalidate();
    }
    W_DIRTY = false;
    W_BLK = u64::MAX;
}

/// Buffer a write of `count` bytes at file offset `off` for NUL-terminated path
/// `full`. Sequential small writes within one 4 KiB block accumulate; a write to a
/// different block/file flushes the previous buffer first. Returns bytes accepted.
unsafe fn wbuf_write(full: &[u8], off: u64, src: *const u8, count: usize) -> usize {
    let plen = full.iter().position(|&b| b == 0).unwrap_or(full.len());
    let blk = off / CBLK as u64;
    let within = (off % CBLK as u64) as usize;
    if within + count > CBLK {
        // straddles a block boundary — flush + write straight through (rare).
        wbuf_flush();
        let mut wr = 0usize;
        oxfs_pwrite(full.as_ptr(), off, src as *const c_void, count, &mut wr);
        oxfs_flush();
        return wr;
    }
    let cp = core::ptr::addr_of!(W_PATH) as *const u8;
    let same =
        W_BLK == blk && W_PLEN == plen && core::slice::from_raw_parts(cp, plen) == &full[..plen];
    if !same {
        wbuf_flush();
        W_BLK = blk;
        W_PLEN = plen;
        let wp = core::ptr::addr_of_mut!(W_PATH) as *mut u8;
        core::ptr::copy_nonoverlapping(full.as_ptr(), wp, plen);
        *wp.add(plen) = 0; // NUL-terminate: oxfs_pwrite takes a C string, not full[..plen]
        // Preserve any existing bytes in this block (partial overwrite / append).
        let mut rd = 0usize;
        oxfs_pread(
            full.as_ptr(),
            blk * CBLK as u64,
            core::ptr::addr_of_mut!(W_BUF) as *mut c_void,
            CBLK,
            &mut rd,
        );
        W_LEN = rd;
    }
    core::ptr::copy_nonoverlapping(src, (core::ptr::addr_of_mut!(W_BUF) as *mut u8).add(within), count);
    if within + count > W_LEN {
        W_LEN = within + count;
    }
    W_DIRTY = true;
    count
}

/// Flush any buffered write to disk (before reads/opens/sync see the file).
fn sync_writes() {
    unsafe {
        wbuf_flush();
    }
}

/// Allocate a transfer frame, map it, and hand it to the block service so reads
/// and writes move whole sectors in one IPC instead of ~13 byte-stream messages.
fn blk_attach() {
    if let Ok(frame) = rt::sys_frame_alloc(BOOT_MEM) {
        if rt::sys_frame_map(frame, FSD_XFER as u64, PROT_READ | PROT_WRITE).is_ok() {
            let mut m = MsgBuf::new(TAG_BLK_ATTACH);
            m.handle_count = 1;
            m.handles[0] = frame;
            if rt::sys_call(BOOT_BLK_EP, &mut m).is_ok() && m.data[0] == 0 {
                unsafe { SHARED_OK = true };
            }
        }
    }
}

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}
/// Print a seed-progress line: "[fsd] seeding NN%".
fn wpct(pct: i64) {
    w(b"[fsd] seeding ");
    let mut v = if pct < 0 { 0u64 } else { pct as u64 };
    let mut b = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    w(&b[i..]);
    w(b"%\n");
}

fn wn(label: &[u8], n: i64) {
    w(label);
    let neg = n < 0;
    let mut v = if neg { (-n) as u64 } else { n as u64 };
    let mut b = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        b[i] = b'-';
    }
    w(&b[i..]);
    w(b"\n");
}

// --- Physical block I/O via the virtio-blk service (byte-stream protocol) ----
fn read_sector(sector: u64, buf: &mut [u8]) -> bool {
    let mut off = 0usize;
    while off < SECTOR {
        let mut m = MsgBuf::new(TAG_BLK_READ);
        m.data[0] = sector;
        m.data[1] = off as u64;
        m.data_len = 2;
        if rt::sys_call(BOOT_BLK_EP, &mut m).is_err() {
            return false;
        }
        let n = (m.data[0] as usize).min(SECTOR - off);
        if n == 0 {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                (m.data.as_ptr() as *const u8).add(8),
                buf.as_mut_ptr().add(off),
                n,
            );
        }
        off += n;
    }
    true
}
fn write_sector(sector: u64, buf: &[u8]) -> bool {
    let mut off = 0usize;
    while off < SECTOR {
        let n = BLK_CHUNK.min(SECTOR - off);
        let mut m = MsgBuf::new(TAG_BLK_WRITE);
        m.data[0] = sector;
        m.data[1] = off as u64;
        m.data[2] = n as u64;
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr().add(off),
                (m.data.as_mut_ptr() as *mut u8).add(24),
                n,
            );
        }
        m.data_len = 8;
        if rt::sys_call(BOOT_BLK_EP, &mut m).is_err() || m.data[0] as usize != n {
            return false;
        }
        off += n;
    }
    true
}

#[no_mangle]
pub extern "C" fn ox_open(_b: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub extern "C" fn ox_close(_b: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub extern "C" fn ox_bread(_b: *mut c_void, buf: *mut u8, blk_id: u64, blk_cnt: u32) -> c_int {
    if unsafe { SHARED_OK } {
        let total = blk_cnt as u64;
        let mut done = 0u64;
        while done < total {
            let chunk = (total - done).min(BLK_XFER_SECTORS);
            let mut m = MsgBuf::new(TAG_BLK_READN);
            m.data[0] = blk_id + done;
            m.data[1] = chunk;
            m.data_len = 2;
            if rt::sys_call(BOOT_BLK_EP, &mut m).is_err() || m.data[0] != 0 {
                return -5;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    FSD_XFER as *const u8,
                    buf.add(done as usize * SECTOR),
                    chunk as usize * SECTOR,
                );
            }
            done += chunk;
        }
        return 0;
    }
    for i in 0..blk_cnt as u64 {
        let dst = unsafe { core::slice::from_raw_parts_mut(buf.add(i as usize * SECTOR), SECTOR) };
        if !read_sector(blk_id + i, dst) {
            return -5;
        }
    }
    0
}
#[no_mangle]
pub extern "C" fn ox_bwrite(_b: *mut c_void, buf: *const u8, blk_id: u64, blk_cnt: u32) -> c_int {
    if unsafe { SHARED_OK } {
        let total = blk_cnt as u64;
        let mut done = 0u64;
        while done < total {
            let chunk = (total - done).min(BLK_XFER_SECTORS);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf.add(done as usize * SECTOR),
                    FSD_XFER as *mut u8,
                    chunk as usize * SECTOR,
                );
            }
            let mut m = MsgBuf::new(TAG_BLK_WRITEN);
            m.data[0] = blk_id + done;
            m.data[1] = chunk;
            m.data_len = 2;
            if rt::sys_call(BOOT_BLK_EP, &mut m).is_err() || m.data[0] != 0 {
                return -5;
            }
            done += chunk;
        }
        return 0;
    }
    for i in 0..blk_cnt as u64 {
        let src = unsafe { core::slice::from_raw_parts(buf.add(i as usize * SECTOR), SECTOR) };
        if !write_sector(blk_id + i, src) {
            return -5;
        }
    }
    let mut m = MsgBuf::new(TAG_BLK_FLUSH);
    let _ = rt::sys_call(BOOT_BLK_EP, &mut m);
    0
}

extern "C" {
    fn oxblk_get() -> *mut c_void;
    fn ext4_device_register(bd: *mut c_void, name: *const u8) -> c_int;
    fn ext4_mount(dev: *const u8, mp: *const u8, ro: bool) -> c_int;
    fn oxfs_mkfs_ext2(bd: *mut c_void) -> c_int;
    fn oxfs_stat(path: *const u8, is_dir: *mut c_int, size: *mut u64) -> c_int;
    fn oxfs_pread(path: *const u8, off: u64, buf: *mut c_void, len: usize, rd: *mut usize) -> c_int;
    // §perf: like oxfs_pread but holds the file open across calls (sequential block reads
    // skip the per-block path walk); oxfs_read_close drops the held handle on mutation.
    fn oxfs_pread2(path: *const u8, off: u64, buf: *mut c_void, len: usize, rd: *mut usize)
        -> c_int;
    fn oxfs_read_close();
    fn oxfs_pwrite(path: *const u8, off: u64, buf: *const c_void, len: usize, wr: *mut usize)
        -> c_int;
    fn oxfs_create(path: *const u8) -> c_int;
    fn oxfs_mkdir(path: *const u8) -> c_int;
    fn oxfs_remove(path: *const u8) -> c_int;
    fn oxfs_rename(path: *const u8, new_path: *const u8) -> c_int;
    fn oxfs_times(path: *const u8, mtime: *mut u32, atime: *mut u32) -> c_int;
    fn oxfs_truncate(path: *const u8, size: u64) -> c_int;
    fn oxfs_set_times(path: *const u8, mtime: u32, atime: u32, set_m: c_int, set_a: c_int) -> c_int;
    fn oxfs_statx2(
        path: *const u8,
        kind: *mut c_int,
        size: *mut u64,
        mtime: *mut u32,
        atime: *mut u32,
    ) -> c_int;
    fn oxfs_symlink(target: *const u8, linkpath: *const u8) -> c_int;
    fn oxfs_readlink(path: *const u8, buf: *mut u8, bufsize: usize, rcnt: *mut usize) -> c_int;
    fn oxfs_link(src: *const u8, dst: *const u8) -> c_int;
    fn oxfs_flush() -> c_int;
    fn oxfs_writeback(on: c_int) -> c_int;
    fn oxfs_readdir(
        path: *const u8,
        cursor: u32,
        name_out: *mut u8,
        cap: u32,
        is_dir: *mut c_int,
    ) -> c_int;
}

// --- id -> path table (the capability/badge bridge) -------------------------
// PATHS[id] is a node's path RELATIVE to the ext2 root (no leading '/'); the full
// lwext4 path is "/mp/" + PATHS[id]. id 1 (FS_ROOT) is the root ("").
const MAXID: usize = 512;
const PLEN: usize = 200;
static mut PATHS: [[u8; PLEN]; MAXID] = [[0; PLEN]; MAXID];
static mut PLENS: [u16; MAXID] = [0; MAXID];
static mut USED: [bool; MAXID] = [false; MAXID];
// Live capability references per intern id: bumped when OPEN mints a cap, dropped on
// TAG_FS_RELEASE (client File/ReadDir close). The slot is reclaimed when this hits 0, so
// only CONCURRENTLY-open paths occupy the table (sequential opens reuse slots).
static mut REFS: [u16; MAXID] = [0; MAXID];
// A NAMESPACE node (TAG_FS_NAMESPACE): everything resolves under its home path
// (PATHS[id]) EXCEPT the top-level dirs root's access rules mounted into it (a MOUNT,
// below) — those resolve against the real fs root. The per-user session root, inherited
// by spawned programs. So homes stay private; mounts (e.g. read-only /bin) are shared.
static mut NS: [bool; MAXID] = [false; MAXID];

// The mount table (TAG_FS_NS_MOUNT): each row makes a path PREFIX NM_NAME (one or
// more components, e.g. `bin` or `projects/oxbow`, or a single file) resolve against
// the fs root for namespace NM_NS (vs its home), at the granted RIGHTS NM_RIGHT.
// Composed at login from root's access rules — this is "root decides who reaches what".
const MAX_NSMNT: usize = 128;
const NM_NAMELEN: usize = 96; // a path prefix, not just a top-level component
static mut NM_USED: [bool; MAX_NSMNT] = [false; MAX_NSMNT];
static mut NM_NS: [u16; MAX_NSMNT] = [0; MAX_NSMNT];
static mut NM_NAME: [[u8; NM_NAMELEN]; MAX_NSMNT] = [[0; NM_NAMELEN]; MAX_NSMNT];
static mut NM_NLEN: [u8; MAX_NSMNT] = [0; MAX_NSMNT];
static mut NM_RIGHT: [u8; MAX_NSMNT] = [0; MAX_NSMNT]; // an FS_RIGHT_* value

// A capability's badge carries its RIGHTS (an FS_RIGHT_* value) in bits 28-30 — node
// ids are < MAXID (512, 9 bits) so the high bits are free. RW (0) leaves the badge =
// node id (backward-compatible). The rights travel WITH the capability: every
// mutating/reading op through it is checked, and any child cap it opens INHERITS the
// rights — so a restriction holds down the whole subtree, including writes through an
// already-open handle.
const RIGHT_SHIFT: u64 = 28;

fn badge_make(id: usize, right: u8) -> u64 {
    id as u64 | ((right as u64) << RIGHT_SHIFT)
}
fn badge_id(b: u64) -> usize {
    (b & ((1 << RIGHT_SHIFT) - 1)) as usize
}
fn badge_right(b: u64) -> u8 {
    ((b >> RIGHT_SHIFT) & 0x7) as u8
}

// ---- per-operation permission, given a right (an FS_RIGHT_* value) ----
fn allows_read(r: u8) -> bool {
    r as u64 != FS_RIGHT_LIST // everything but LIST can read file content
}
fn allows_create(r: u8) -> bool {
    matches!(r as u64, FS_RIGHT_RW | FS_RIGHT_APPEND | FS_RIGHT_NODELETE)
}
fn allows_overwrite(r: u8) -> bool {
    matches!(r as u64, FS_RIGHT_RW | FS_RIGHT_NODELETE) // write@offset<size, truncate
}
fn allows_append(r: u8) -> bool {
    matches!(r as u64, FS_RIGHT_RW | FS_RIGHT_APPEND | FS_RIGHT_NODELETE) // write@end
}
fn allows_mkdir(r: u8) -> bool {
    matches!(r as u64, FS_RIGHT_RW | FS_RIGHT_NODELETE)
}
fn allows_delete(r: u8) -> bool {
    r as u64 == FS_RIGHT_RW // unlink/rmdir/rename: full RW only
}
fn allows_link(r: u8) -> bool {
    matches!(r as u64, FS_RIGHT_RW | FS_RIGHT_NODELETE)
}

fn rel(id: usize) -> &'static [u8] {
    unsafe { &PATHS[id][..PLENS[id] as usize] }
}

/// Do `name`'s leading path components match `prefix`'s components exactly? (So mount
/// `projects/oxbow` matches `projects/oxbow/x` and `projects/oxbow`, but NOT `projects`
/// or `projectsX`.) Empty components are skipped on both sides.
fn comp_prefix(name: &[u8], prefix: &[u8]) -> bool {
    let mut n = name.split(|&b| b == b'/').filter(|c| !c.is_empty());
    let p = prefix.split(|&b| b == b'/').filter(|c| !c.is_empty());
    for pc in p {
        match n.next() {
            Some(nc) if nc == pc => continue,
            _ => return false,
        }
    }
    true
}

/// If `name` falls under a mount of namespace `id`, return Some(its rights). A mounted
/// prefix resolves against the fs root (shared) instead of home (private). Longest
/// matching prefix wins (a deeper mount can override a shallower one).
fn ns_mount_for(id: usize, name: &[u8]) -> Option<u8> {
    if !unsafe { NS[id] } {
        return None;
    }
    let mut best: Option<(usize, u8)> = None; // (prefix len in bytes, right)
    for i in 0..MAX_NSMNT {
        unsafe {
            if NM_USED[i] && NM_NS[i] as usize == id {
                let prefix = &NM_NAME[i][..NM_NLEN[i] as usize];
                if comp_prefix(name, prefix) {
                    let plen = prefix.len();
                    if best.map_or(true, |(bl, _)| plen > bl) {
                        best = Some((plen, NM_RIGHT[i]));
                    }
                }
            }
        }
    }
    best.map(|(_, r)| r)
}

/// The effective right for a NAME-based op through cap (`id`, `cap_right`): the mount's
/// right if `name` is mounted, else the cap's inherited right (home/confined subtree).
fn eff_right(id: usize, cap_right: u8, name: &[u8]) -> u8 {
    ns_mount_for(id, name).unwrap_or(cap_right)
}

/// Absolute lwext4 path ("/mp/" + relpath) into `out`, NUL-terminated. An empty
/// relpath yields "/mp/" (the mount root). Returns the length, or None if it
/// doesn't fit.
fn full_from_rel(relpath: &[u8], out: &mut [u8; 256]) -> Option<usize> {
    let total = 4 + relpath.len();
    if total + 1 > out.len() {
        return None;
    }
    out[..4].copy_from_slice(b"/mp/");
    out[4..total].copy_from_slice(relpath);
    out[total] = 0;
    Some(total)
}

fn full_path(id: usize, out: &mut [u8; 256]) -> Option<usize> {
    full_from_rel(rel(id), out)
}

/// Join parent `id`'s rel-path with `name` (a possibly multi-component, possibly
/// slash-padded name) into `relbuf`, NORMALIZED: empty components collapsed, no
/// leading/trailing slash. The result length may equal the parent's (name had no
/// real components, e.g. "/") — then the child IS the parent directory. Returns
/// the child rel length, or None if it overflows. Caller built `out` via
/// full_from_rel(&relbuf[..len]).
fn join_child(id: usize, name: &[u8], relbuf: &mut [u8; PLEN]) -> Option<usize> {
    // A namespace node routes a MOUNTED top-level dir against the fs root (base = "")
    // and everything else under its home (base = the node's path). name_ok already
    // blocks ".."/"." so neither subtree can be escaped.
    let p: &[u8] = if unsafe { NS[id] } {
        if ns_mount_for(id, name).is_some() {
            &[]
        } else {
            rel(id)
        }
    } else {
        rel(id)
    };
    let mut len = 0usize;
    if !p.is_empty() {
        relbuf[..p.len()].copy_from_slice(p);
        len = p.len();
    }
    for comp in name.split(|&b| b == b'/') {
        if comp.is_empty() {
            continue; // collapse //, leading/trailing slash (".."/"." rejected upstream)
        }
        if len > 0 {
            if len + 1 > PLEN {
                return None;
            }
            relbuf[len] = b'/';
            len += 1;
        }
        if len + comp.len() > PLEN {
            return None;
        }
        relbuf[len..len + comp.len()].copy_from_slice(comp);
        len += comp.len();
    }
    Some(len)
}

/// Find an existing id for `relpath`, or allocate a fresh one. 0 = table full.
fn intern(relpath: &[u8]) -> usize {
    unsafe {
        // Dedup starts at node 2: node 1 is FS_ROOT (the root-authority badge) and must
        // NEVER be handed out by interning a path (esp. the empty path) — only the boot
        // grant holds it. (OPEN handles "open-self" without interning; see TAG_FS_OPEN.)
        for i in 2..MAXID {
            if USED[i] && &PATHS[i][..PLENS[i] as usize] == relpath {
                return i;
            }
        }
        for i in 2..MAXID {
            if !USED[i] {
                PATHS[i][..relpath.len()].copy_from_slice(relpath);
                PLENS[i] = relpath.len() as u16;
                USED[i] = true;
                return i;
            }
        }
    }
    0
}

/// Allocate a FRESH node for `relpath` WITHOUT deduping. Every namespace gets an
/// independent identity (hence its own mount set), even when two namespaces share the
/// same home path — e.g. a `confine`/profile namespace alongside the session one. (The
/// interning dedup is right for FILES but wrong for namespaces, which must not share.)
fn fresh_node(relpath: &[u8]) -> usize {
    unsafe {
        for i in 2..MAXID {
            if !USED[i] {
                PATHS[i][..relpath.len()].copy_from_slice(relpath);
                PLENS[i] = relpath.len() as u16;
                USED[i] = true;
                return i;
            }
        }
    }
    0
}

/// Drop every mount belonging to namespace node `id` (so a reused node slot never
/// inherits a previous namespace's mounts).
fn ns_clear_mounts(id: usize) {
    unsafe {
        for i in 0..MAX_NSMNT {
            if NM_USED[i] && NM_NS[i] as usize == id {
                NM_USED[i] = false;
            }
        }
    }
}

/// Release the intern slot for a now-removed `relpath`, so the fixed-size `PATHS`
/// table is reclaimed instead of leaking. If a client still holds a live cap to this id
/// (REFS > 0 — an open File across its own unlink), DO NOT free the slot: reusing it for
/// a different path would let that stale cap alias the new (possibly cross-user) path.
/// The slot is left interned — the on-disk file is already gone, so the stale cap just
/// resolves to a deleted path (NotFound) — and `release_intern` reclaims it when the
/// last cap closes.
fn free_intern(relpath: &[u8]) {
    unsafe {
        for i in 2..MAXID {
            if USED[i] && &PATHS[i][..PLENS[i] as usize] == relpath {
                if REFS[i] == 0 {
                    USED[i] = false;
                    PLENS[i] = 0;
                }
                return;
            }
        }
    }
}

/// Drop one live-cap reference to intern id `id`; reclaim the slot at zero. Called from
/// the TAG_FS_RELEASE handler when a client closes a File/ReadDir cap.
fn release_intern(id: usize) {
    unsafe {
        if id >= 2 && id < MAXID && USED[id] && REFS[id] > 0 {
            REFS[id] -= 1;
            if REFS[id] == 0 {
                USED[id] = false;
                PLENS[id] = 0;
                // A namespace node: drop its flag + mounts so a later reuse of this slot
                // starts clean (and a logout/login with changed rules sees no stale mount).
                if NS[id] {
                    NS[id] = false;
                    ns_clear_mounts(id);
                }
            }
        }
    }
}

/// A path component is single and safe: non-empty, no '/', not '.' or '..'
/// (capability confinement — a dir cap cannot walk above its subtree, L3).
fn name_ok(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    for comp in name.split(|&b| b == b'/') {
        if comp == b".." || comp == b"." {
            return false;
        }
    }
    true
}

/// Extract a NUL-terminated name from a message's data bytes (max 64).
fn msg_name(m: &MsgBuf) -> &[u8] {
    let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
    let n = bytes.iter().position(|&b| b == 0).unwrap_or(0);
    &bytes[..n]
}

// --- USTAR initrd seeding (first format only) -------------------------------
fn parse_octal(b: &[u8]) -> usize {
    let mut v = 0usize;
    let mut started = false;
    for &c in b {
        if (b'0'..=b'7').contains(&c) {
            v = v * 8 + (c - b'0') as usize;
            started = true;
        } else if started {
            break;
        }
    }
    v
}

/// Ensure all ancestor directories of rel path exist (mkdir -p), then return the
/// full path NUL-term in `out`.
fn ensure_dirs(relpath: &[u8]) {
    // create each prefix dir
    let mut i = 0;
    while let Some(slash) = relpath[i..].iter().position(|&b| b == b'/') {
        let end = i + slash;
        if end > 0 {
            let mut out = [0u8; 256];
            out[..4].copy_from_slice(b"/mp/");
            out[4..4 + end].copy_from_slice(&relpath[..end]);
            out[4 + end] = 0;
            unsafe { oxfs_mkdir(out.as_ptr()) };
        }
        i = end + 1;
    }
}

fn flush() {
    unsafe {
        let _ = oxfs_flush();
    }
}

fn seed_from_initrd() {
    w(b"[fsd] seeding ext2 from the initrd...\n");
    // Write-back is toggled PER FILE in the loop below (batch a file, then drain), not
    // once for the whole seed — see the per-file rationale there.
    let base = FS_INITRD as *const u8;
    let mut off = 0usize;
    let mut files = 0u32;
    // Pre-walk the headers (cheap — header parsing only, no ext2 writes) to learn the
    // total tar length, so the slow write loop below can report a progress percentage
    // by bytes processed (a good proxy for time, since writes dominate).
    let total = {
        let mut o = 0usize;
        loop {
            let h = unsafe { base.add(o) };
            if unsafe { *h } == 0 {
                break;
            }
            let sz = parse_octal(unsafe { core::slice::from_raw_parts(h.add(124), 12) });
            o += 512 + ((sz + 511) & !511);
        }
        if o == 0 { 1 } else { o }
    };
    let mut next_mark = 10i64;
    loop {
        let hdr = unsafe { base.add(off) };
        if unsafe { *hdr } == 0 {
            break;
        }
        let name_raw = unsafe { core::slice::from_raw_parts(hdr, 100) };
        let nlen = name_raw.iter().position(|&b| b == 0).unwrap_or(100);
        let mut nm = &name_raw[..nlen];
        if nm.starts_with(b"./") {
            nm = &nm[2..];
        }
        let trailing = nm.ends_with(b"/");
        if trailing {
            nm = &nm[..nm.len() - 1];
        }
        let size = parse_octal(unsafe { core::slice::from_raw_parts(hdr.add(124), 12) });
        let typeflag = unsafe { *hdr.add(156) };
        let is_dir = typeflag == b'5' || trailing;
        let is_file = !is_dir && (typeflag == b'0' || typeflag == 0);
        // The full tree (incl. the megabyte source under /usr/src) is now seeded —
        // the shared-memory block transfer makes it fast enough.
        if !nm.is_empty() && nm.len() < PLEN {
            ensure_dirs(nm);
            let mut full = [0u8; 256];
            full[..4].copy_from_slice(b"/mp/");
            full[4..4 + nm.len()].copy_from_slice(nm);
            full[4 + nm.len()] = 0;
            if is_dir {
                unsafe { oxfs_mkdir(full.as_ptr()) };
            } else if is_file {
                if unsafe { oxfs_create(full.as_ptr()) } == 0 {
                    // Batch THIS file's blocks in write-back mode, then drain with the
                    // clean writeback toggle. Per-file (not whole-seed) batching keeps the
                    // dirty set ≤ one file's blocks, so with the bumped cache (1024 blocks)
                    // lwext4 never has to evict mid-write — avoiding both the silent
                    // truncation AND the LRU-tree corruption (ext4_buf_lru_RB_REMOVE
                    // null-deref) that an unbounded write-back accumulation + ext4_cache_flush
                    // triggered. writeback(0) flushes cleanly (same call used at seed end).
                    unsafe { oxfs_writeback(1) };
                    let data = unsafe { hdr.add(512) };
                    let mut done = 0usize;
                    while done < size {
                        let chunk = core::cmp::min(size - done, 4096);
                        let mut wr = 0usize;
                        unsafe {
                            oxfs_pwrite(
                                full.as_ptr(),
                                done as u64,
                                data.add(done) as *const c_void,
                                chunk,
                                &mut wr,
                            )
                        };
                        if wr == 0 {
                            break;
                        }
                        done += wr;
                    }
                    unsafe { oxfs_writeback(0) }; // flush this file cleanly before the next
                    files += 1;
                }
            }
        }
        off += 512 + ((size + 511) & !511);
        // Report progress as each 10% boundary is crossed (skips multiple at once if
        // a big file jumps the offset). The "seeded files: N" line marks 100%.
        let pct = (off as u64 * 100 / total as u64) as i64;
        while pct >= next_mark && next_mark <= 90 {
            wpct(next_mark);
            next_mark += 10;
        }
    }
    unsafe { oxfs_writeback(0) }; // flush + back to write-through
    wn(b"[fsd] seeded files: ", files as i64);
}

fn reply_status(reply: Handle, status: u64) {
    let mut r = MsgBuf::new(0);
    r.data[0] = status;
    r.data_len = 1;
    let _ = rt::sys_reply(reply, &r);
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[fsd] ext2 filesystem server starting\n");
    blk_attach();
    if unsafe { SHARED_OK } {
        w(b"[fsd] shared-memory block transfer attached\n");
    } else {
        w(b"[fsd] WARN: bulk attach failed, using slow byte-stream\n");
    }
    unsafe {
        let bd = oxblk_get();
        ext4_device_register(bd, b"ox\0".as_ptr());
        let mut r = ext4_mount(b"ox\0".as_ptr(), b"/mp/\0".as_ptr(), false);
        if r != 0 {
            w(b"[fsd] formatting + seeding the disk (first boot)...\n");
            if oxfs_mkfs_ext2(bd) == 0 {
                r = ext4_mount(b"ox\0".as_ptr(), b"/mp/\0".as_ptr(), false);
                if r == 0 {
                    seed_from_initrd();
                }
            }
        } else {
            w(b"[fsd] mounted existing ext2 from disk\n");
        }
        if r != 0 {
            w(b"[fsd] FATAL: could not mount ext2\n");
            rt::sys_exit(1);
        }
    }
    // The root node id.
    unsafe {
        USED[FS_ROOT as usize] = true;
        PLENS[FS_ROOT as usize] = 0;
    }
    w(b"[fsd] ready (ext2 on virtio-blk)\n");

    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_EP, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let cap_right = badge_right(m.badge); // the rights this capability carries
        let id = badge_id(m.badge);
        let valid = id > 0 && id < MAXID && unsafe { USED[id] };

        match m.tag {
            TAG_FS_OPEN => {
                // Flags-driven open: fsd applies the full OpenOptions semantics here (in
                // one round trip) so the client needs no separate stat. data[63] = FS_O_*.
                sync_writes(); // a buffered write must be on disk before we stat/serve
                let flags = m.data[MSG_DATA_WORDS - 1];
                let name = msg_name(&m);
                let mut full = [0u8; 256];
                let mut relbuf = [0u8; PLEN];
                let mut r = MsgBuf::new(0);
                let mut status = 1u64; // default NotFound
                let mut done = false; // set once we've replied with the cap
                'open: {
                    if !(valid && name_ok(name)) {
                        break 'open;
                    }
                    // §namespace: the effective rights at this name (the mount's, or the
                    // cap's inherited rights) gate create/truncate.
                    let er = eff_right(id, cap_right, name);
                    if flags & FS_O_CREATE != 0 && !allows_create(er) {
                        status = 3; // create denied by rights
                        break 'open;
                    }
                    if flags & FS_O_TRUNC != 0 && !allows_overwrite(er) {
                        status = 3; // truncate denied by rights
                        break 'open;
                    }
                    let Some(clen) = join_child(id, name, &mut relbuf) else { break 'open };
                    if full_from_rel(&relbuf[..clen], &mut full).is_none() {
                        break 'open;
                    }
                    let mut kind = 0;
                    let mut size = 0u64;
                    let mut mtime = 0u32;
                    let mut atime = 0u32;
                    let existed = unsafe {
                        oxfs_statx2(full.as_ptr(), &mut kind, &mut size, &mut mtime, &mut atime)
                    } == 0;

                    // A directory can be opened read-only (flags=0, for readdir) but never
                    // created/truncated or opened for write — `File::create` on a directory
                    // must fail (EISDIR), like unix.
                    if existed && kind == 1 && flags & (FS_O_CREATE | FS_O_TRUNC) != 0 {
                        status = 3; // IsADirectory
                        break 'open;
                    }
                    if flags & FS_O_EXCL != 0 && existed {
                        status = 2; // AlreadyExists (O_EXCL)
                        break 'open;
                    }
                    if !existed && flags & FS_O_CREATE == 0 {
                        status = 1; // NotFound, no O_CREAT
                        break 'open;
                    }
                    if !existed {
                        // create-or-truncate makes an empty regular file
                        if unsafe { oxfs_create(full.as_ptr()) } != 0 {
                            break 'open;
                        }
                        flush();
                        cache_invalidate();
                        // The node is now a fresh empty file; read just its (cheap) times
                        // rather than a second full statx2 (which would re-open it for fsize).
                        kind = 2; // FS_FILE
                        size = 0;
                        unsafe { oxfs_times(full.as_ptr(), &mut mtime, &mut atime) };
                    } else if flags & FS_O_TRUNC != 0 {
                        if unsafe { oxfs_truncate(full.as_ptr(), 0) } == 0 {
                            flush();
                            cache_invalidate();
                            size = 0;
                        }
                    }

                    // "open `/` (or `.`)" yields a cap to the SAME node, NOT a re-intern of
                    // the empty path — which `intern` would resolve to node 1 (FS_ROOT),
                    // forging the root-authority badge for any base-empty cap (e.g. a
                    // namespace `confine`d at the disk root). Reusing `id` keeps open-self
                    // correct without ever minting FS_ROOT.
                    let cid = if clen == 0 { id } else { intern(&relbuf[..clen]) };
                    if cid == 0 {
                        break 'open;
                    }
                    // Propagate rights down: a cap opened in a restricted location carries
                    // those rights, so ops through it AND anything it opens are governed.
                    let badge = badge_make(cid, er);
                    let Ok(cap) = rt::sys_mint(BOOT_EP, badge, R_SEND | R_GRANT) else {
                        break 'open;
                    };
                    // Live cap for `cid`; reclaimed only when its FS_RELEASE brings refs to 0.
                    unsafe { REFS[cid] += 1 };
                    let kind_tag = match kind {
                        1 => FS_DIR,
                        3 => FS_SYMLINK,
                        _ => FS_FILE,
                    };
                    r.data[0] = 0; // ok
                    r.data[1] = kind_tag;
                    r.data[2] = size;
                    r.data[3] = mtime as u64;
                    r.data[4] = atime as u64;
                    r.data_len = 5;
                    r.handle_count = 1;
                    r.handles[0] = cap;
                    let _ = rt::sys_reply(reply, &r);
                    let _ = rt::sys_close(cap);
                    done = true;
                }
                if !done {
                    reply_status(reply, status);
                }
            }
            TAG_FS_NAMESPACE => {
                // Mint a namespace cap rooted at the given home path (e.g. "home/bryson"):
                // everything resolves under home until root MOUNTS top-level dirs into it
                // (TAG_FS_NS_MOUNT). Inherited as a confined user's session root.
                let name = msg_name(&m);
                let home = name.strip_prefix(b"/").unwrap_or(name); // "" = the ext2 root
                let mut r = MsgBuf::new(0);
                let mut status = 1u64;
                'ns: {
                    // PRIVILEGED: creating a namespace (esp. one rooted at the disk root with
                    // full rights) is a root-authority operation. Only the holder of the
                    // FS_ROOT cap (the login shell's BOOT_FS_ROOT) may do it — a confined
                    // user's home/file caps carry a different badge and are rejected here.
                    if id != FS_ROOT as usize {
                        break 'ns;
                    }
                    if home.len() >= PLEN || !name_ok(if home.is_empty() { b"x" } else { home }) {
                        break 'ns;
                    }
                    // A FRESH node (not interned): each namespace is independent, so a
                    // confine/profile namespace doesn't share the session's mounts even at
                    // the same home path.
                    let cid = fresh_node(home);
                    if cid == 0 {
                        break 'ns;
                    }
                    unsafe {
                        NS[cid] = true;
                        ns_clear_mounts(cid); // defensive: a reused slot starts with no mounts
                    }
                    let Ok(cap) = rt::sys_mint(BOOT_EP, cid as u64, R_SEND | R_GRANT) else {
                        break 'ns;
                    };
                    unsafe { REFS[cid] += 1 };
                    r.data[0] = 0;
                    r.data[1] = FS_DIR;
                    r.data[2] = cid as u64; // the namespace node id — used to target NS_MOUNT
                    r.data_len = 3;
                    r.handle_count = 1;
                    r.handles[0] = cap;
                    let _ = rt::sys_reply(reply, &r);
                    let _ = rt::sys_close(cap);
                    status = 0;
                }
                if status != 0 {
                    reply_status(reply, status);
                }
            }
            TAG_FS_NS_MOUNT => {
                // PRIVILEGED: mounting a path prefix into a namespace (at attacker-chosen
                // rights) is the "root decides who reaches what" primitive — it MUST be a
                // root-authority op, else a confined user could mount the whole disk RW
                // into their own session namespace. So this is sent to the FS_ROOT cap
                // (the shell's BOOT_FS_ROOT); the target namespace node is named in
                // data[0] (returned by TAG_FS_NAMESPACE). The prefix `name` resolves
                // against the fs root (shared) instead of home, at the rights in data[63].
                let name = msg_name(&m);
                let right = (m.data[63] & 0x7) as u8;
                // Target node id lives in data[62] — clear of the name (words 0..7) and the
                // rights (word 63), both of which the kernel transmits (data_len=64).
                let target = m.data[62] as usize;
                let mut status = 1u64;
                'mnt: {
                    // Caller must hold FS_ROOT; target must be a real namespace node.
                    if id != FS_ROOT as usize {
                        break 'mnt;
                    }
                    if !(target > 0 && target < MAXID && unsafe { USED[target] && NS[target] } && name_ok(name)) {
                        break 'mnt;
                    }
                    // Normalise the prefix: drop empty components, join with single '/'.
                    let mut pbuf = [0u8; NM_NAMELEN];
                    let mut plen = 0usize;
                    for c in name.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
                        if plen != 0 {
                            if plen >= NM_NAMELEN {
                                break 'mnt;
                            }
                            pbuf[plen] = b'/';
                            plen += 1;
                        }
                        if plen + c.len() > NM_NAMELEN {
                            break 'mnt;
                        }
                        pbuf[plen..plen + c.len()].copy_from_slice(c);
                        plen += c.len();
                    }
                    if plen == 0 {
                        break 'mnt;
                    }
                    let prefix = &pbuf[..plen];
                    // Idempotent: if already mounted in this ns, just update the rights.
                    let mut slot = None;
                    for i in 0..MAX_NSMNT {
                        unsafe {
                            if NM_USED[i]
                                && NM_NS[i] as usize == target
                                && &NM_NAME[i][..NM_NLEN[i] as usize] == prefix
                            {
                                slot = Some(i);
                                break;
                            }
                        }
                    }
                    let i = match slot {
                        Some(i) => i,
                        None => {
                            let mut free = None;
                            for j in 0..MAX_NSMNT {
                                if !unsafe { NM_USED[j] } {
                                    free = Some(j);
                                    break;
                                }
                            }
                            let Some(j) = free else { break 'mnt };
                            unsafe {
                                NM_USED[j] = true;
                                NM_NS[j] = target as u16;
                                NM_NLEN[j] = plen as u8;
                                NM_NAME[j][..plen].copy_from_slice(prefix);
                            }
                            j
                        }
                    };
                    unsafe { NM_RIGHT[i] = right };
                    status = 0;
                }
                reply_status(reply, status);
            }
            TAG_FS_READ => {
                sync_writes(); // §94: reads must see buffered writes
                let off = m.data[0];
                let mut r = MsgBuf::new(0);
                let mut full = [0u8; 256];
                let mut count = 0usize;
                // LIST rights can enumerate names (readdir) but not read file content.
                if valid && allows_read(cap_right) && full_path(id, &mut full).is_some() {
                    let dst = unsafe { (r.data.as_mut_ptr() as *mut u8).add(8) };
                    // §94: served from the one-block cache — collapses the ~73
                    // path-resolved disk reads per 4 KiB into one.
                    count = unsafe { cached_read(&full, off, dst) };
                }
                r.data[0] = count as u64;
                r.data_len = 64; // all MSG_DATA_WORDS valid (count + up to 504 payload bytes)
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_READ_BULK => {
                // §perf: read up to one PAGE at `off` into BULKBUF, then deliver it
                // straight into the caller's `dst` buffer via SYS_REPLY_BULK — one round
                // trip per 4 KiB instead of ~8 inline 504-B TAG_FS_READ round trips.
                sync_writes();
                let off = m.data[0];
                let want = (m.data[1] as usize).min(4096);
                let dst = m.data[2]; // the caller's destination buffer vaddr
                let mut full = [0u8; 256];
                let mut total = 0usize;
                let mut served = false;
                if valid && allows_read(cap_right) && full_path(id, &mut full).is_some() {
                    unsafe {
                        let buf = core::ptr::addr_of_mut!(BULKBUF) as *mut u8;
                        while total < want {
                            let got = cached_read(&full, off + total as u64, buf.add(total));
                            if got == 0 {
                                break; // EOF
                            }
                            total += got;
                        }
                        let n = total.min(want);
                        let mut r = MsgBuf::new(0);
                        r.data[0] = n as u64;
                        r.data_len = 1;
                        if rt::sys_reply_bulk(reply, &r, buf as *const u8, dst, n as u64).is_ok() {
                            served = true;
                        }
                    }
                }
                if !served {
                    // bad cap/dst, or the bulk copy failed — unblock the caller with count 0.
                    let mut r = MsgBuf::new(0);
                    r.data[0] = 0;
                    r.data_len = 1;
                    let _ = rt::sys_reply(reply, &r);
                }
            }
            TAG_FS_READDIR => {
                let cursor = m.data[0] as u32;
                let mut r = MsgBuf::new(0);
                let mut full = [0u8; 256];
                let mut hit = false;
                if valid && full_path(id, &mut full).is_some() {
                    let dst = unsafe { (r.data.as_mut_ptr() as *mut u8).add(16) };
                    let mut is_dir = 0;
                    if unsafe { oxfs_readdir(full.as_ptr(), cursor, dst, 40, &mut is_dir) } == 0 {
                        r.data[0] = 1;
                        r.data[1] = if is_dir != 0 { FS_DIR } else { FS_FILE };
                        r.data_len = 8;
                        hit = true;
                    }
                }
                if !hit {
                    r.data[0] = 0;
                    r.data_len = 1;
                }
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_CREATE => {
                cache_invalidate(); // §94: keep the read cache from serving stale bytes
                sync_writes();
                let name = msg_name(&m);
                let mut full = [0u8; 256];
                let mut relbuf = [0u8; PLEN];
                let mut r = MsgBuf::new(0);
                let mut done = false;
                let er = eff_right(id, cap_right, name);
                // CREATE is create-OR-TRUNCATE: making a new file needs create rights, but
                // clobbering an existing one needs overwrite rights (so APPEND can't use a
                // truncating `>` to erase a log it may only append to).
                let create_ok = if valid && name_ok(name) {
                    let mut rb = [0u8; PLEN];
                    let mut fp = [0u8; 256];
                    let exists = join_child(id, name, &mut rb)
                        .filter(|&c| c > 0)
                        .map(|c| {
                            full_from_rel(&rb[..c], &mut fp);
                            let (mut k, mut sz, mut mt, mut at) = (0, 0u64, 0u32, 0u32);
                            unsafe { oxfs_statx2(fp.as_ptr(), &mut k, &mut sz, &mut mt, &mut at) == 0 }
                        })
                        .unwrap_or(false);
                    if exists { allows_overwrite(er) } else { allows_create(er) }
                } else {
                    false
                };
                if create_ok {
                    let clen = join_child(id, name, &mut relbuf);
                    if let Some(childlen) = clen {
                        full_from_rel(&relbuf[..childlen], &mut full);
                        if childlen > 0 && unsafe { oxfs_create(full.as_ptr()) } == 0 {
                            flush();
                            let cid = intern(&relbuf[..childlen]);
                            if cid != 0 {
                                let badge = badge_make(cid, eff_right(id, cap_right, name));
                                if let Ok(cap) = rt::sys_mint(BOOT_EP, badge, R_SEND | R_GRANT)
                                {
                                    unsafe { REFS[cid] += 1 }; // live cap; released on close
                                    r.data[0] = 0;
                                    r.data_len = 1;
                                    r.handle_count = 1;
                                    r.handles[0] = cap;
                                    let _ = rt::sys_reply(reply, &r);
                                    let _ = rt::sys_close(cap);
                                    done = true;
                                }
                            }
                        }
                    }
                }
                if !done {
                    reply_status(reply, 1);
                }
            }
            TAG_FS_WRITE => {
                cache_invalidate(); // §94: read cache may hold blocks we're changing
                let off = m.data[0];
                let count = (m.data[1] as usize).min(480); // payload past the 16 B header
                let mut full = [0u8; 256];
                let mut written = 0usize;
                // Rights gate writes: RO/LIST deny entirely; APPEND allows only writes at
                // or past EOF (so a log can't be rewritten); RW/NODELETE allow any offset.
                let mut write_ok = allows_append(cap_right);
                if write_ok && cap_right as u64 == FS_RIGHT_APPEND && full_path(id, &mut full).is_some() {
                    sync_writes(); // flush buffered writes so the size check is accurate
                    let mut kind = 0;
                    let mut size = 0u64;
                    let mut mt = 0u32;
                    let mut at = 0u32;
                    unsafe { oxfs_statx2(full.as_ptr(), &mut kind, &mut size, &mut mt, &mut at) };
                    if off < size {
                        write_ok = false; // append-only: no overwriting existing bytes
                    }
                }
                if valid && write_ok && full_path(id, &mut full).is_some() {
                    let src = unsafe { (m.data.as_ptr() as *const u8).add(16) };
                    // §94: coalesce into 4 KiB blocks instead of open+write+flush
                    // per write — the difference between seconds and minutes for
                    // a multi-MiB file.
                    written = unsafe { wbuf_write(&full, off, src, count) };
                }
                let mut r = MsgBuf::new(0);
                r.data[0] = written as u64;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_MKDIR => {
                sync_writes(); // §94
                let name = msg_name(&m);
                let mut full = [0u8; 256];
                let mut relbuf = [0u8; PLEN];
                let mut status = 1u64;
                if valid && name_ok(name) && allows_mkdir(eff_right(id, cap_right, name)) {
                    if let Some(clen) = join_child(id, name, &mut relbuf) {
                        full_from_rel(&relbuf[..clen], &mut full);
                        if clen > 0 && unsafe { oxfs_mkdir(full.as_ptr()) } == 0 {
                            status = 0;
                            flush();
                        }
                    }
                }
                reply_status(reply, status);
            }
            TAG_FS_UNLINK => {
                cache_invalidate(); // §94
                sync_writes();
                let name = msg_name(&m);
                let mut full = [0u8; 256];
                let mut relbuf = [0u8; PLEN];
                let mut status = 1u64;
                if valid && name_ok(name) && allows_delete(eff_right(id, cap_right, name)) {
                    if let Some(clen) = join_child(id, name, &mut relbuf) {
                        full_from_rel(&relbuf[..clen], &mut full);
                        if clen > 0 && unsafe { oxfs_remove(full.as_ptr()) } == 0 {
                            status = 0;
                            free_intern(&relbuf[..clen]); // reclaim the path-table slot
                            flush();
                        }
                    }
                }
                reply_status(reply, status);
            }
            TAG_FS_RENAME => {
                cache_invalidate(); // §94
                sync_writes();
                // data = old name NUL, then new name NUL — over the valid data region
                // (rt packs up to PLEN bytes per path, not just the old 64-byte window).
                let win = ((m.data_len as usize) * 8).clamp(64, 512); // 512 B inline data area
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, win) };
                let oldlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let old = &bytes[..oldlen];
                let nstart = oldlen + 1;
                let new = if nstart < win {
                    let rest = &bytes[nstart..];
                    let nl = rest.iter().position(|&b| b == 0).unwrap_or(0);
                    &rest[..nl]
                } else {
                    &bytes[0..0]
                };
                let mut of = [0u8; 256];
                let mut nf = [0u8; 256];
                let mut rb1 = [0u8; PLEN];
                let mut rb2 = [0u8; PLEN];
                let mut status = 1u64;
                // Rename = remove old + create new: need delete rights on the source and
                // create rights on the destination.
                if valid && name_ok(old) && name_ok(new)
                    && allows_delete(eff_right(id, cap_right, old))
                    && allows_create(eff_right(id, cap_right, new))
                {
                    let o = join_child(id, old, &mut rb1);
                    let n = join_child(id, new, &mut rb2);
                    if let (Some(ol), Some(nl)) = (o, n) {
                        full_from_rel(&rb1[..ol], &mut of);
                        full_from_rel(&rb2[..nl], &mut nf);
                        if ol > 0 && nl > 0 && unsafe { oxfs_rename(of.as_ptr(), nf.as_ptr()) } == 0 {
                            status = 0;
                            flush();
                        }
                    }
                }
                reply_status(reply, status);
            }
            TAG_FS_SYNC => {
                // §94: commit any buffered write, then the ext2 write-through has
                // everything on disk. So `sync` is a real durability barrier.
                sync_writes();
                let mut r = MsgBuf::new(0);
                r.data[0] = 0;
                r.data[1] = 0;
                r.data_len = 2;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_RELEASE => {
                // A client closed a File/ReadDir cap: drop its reference to the interned
                // path so the slot can be reused. `id` is the cap badge (the intern id).
                if valid {
                    release_intern(id);
                }
                reply_status(reply, 0);
            }
            TAG_FS_TRUNCATE => {
                // set_len on the file behind this capability. Flush buffered writes first
                // so the truncate sees the real on-disk size, then persist the new length.
                sync_writes();
                let size = m.data[0];
                let mut full = [0u8; 256];
                let mut ok = false;
                // Truncate rewrites the file, so it needs overwrite rights (RW/NODELETE).
                if valid && allows_overwrite(cap_right) && full_path(id, &mut full).is_some() {
                    ok = unsafe { oxfs_truncate(full.as_ptr(), size) } == 0;
                    if ok {
                        unsafe { oxfs_flush() };
                        cache_invalidate(); // §94: truncate changed size/content
                    }
                }
                reply_status(reply, if ok { 0 } else { 1 });
            }
            TAG_FS_SETTIMES => {
                let mtime = m.data[0] as u32;
                let atime = m.data[1] as u32;
                let flags = m.data[2];
                let mut full = [0u8; 256];
                let mut ok = false;
                // Setting times is a metadata write — any writable right permits it.
                if valid && allows_append(cap_right) && full_path(id, &mut full).is_some() {
                    let set_m = (flags & 1) as c_int;
                    let set_a = ((flags >> 1) & 1) as c_int;
                    ok = unsafe { oxfs_set_times(full.as_ptr(), mtime, atime, set_m, set_a) } == 0;
                    if ok {
                        unsafe { oxfs_flush() };
                    }
                }
                reply_status(reply, if ok { 0 } else { 1 });
            }
            TAG_FS_SYMLINK => {
                cache_invalidate();
                sync_writes();
                // data = target\0 linkpath\0. The target is stored literally (not resolved);
                // linkpath is resolved against the cwd cap like create.
                let win = ((m.data_len as usize) * 8).clamp(64, 512);
                let bytes =
                    unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, win) };
                let tlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let target = &bytes[..tlen];
                let lstart = tlen + 1;
                let link = if lstart < win {
                    let rest = &bytes[lstart..];
                    let ll = rest.iter().position(|&b| b == 0).unwrap_or(0);
                    &rest[..ll]
                } else {
                    &bytes[0..0]
                };
                let mut lf = [0u8; 256];
                let mut rb = [0u8; PLEN];
                let mut tbuf = [0u8; 256];
                let mut status = 1u64;
                if valid && name_ok(link) && tlen < tbuf.len()
                    && allows_link(eff_right(id, cap_right, link))
                {
                    if let Some(ll) = join_child(id, link, &mut rb) {
                        full_from_rel(&rb[..ll], &mut lf);
                        tbuf[..tlen].copy_from_slice(target);
                        tbuf[tlen] = 0;
                        if ll > 0 && unsafe { oxfs_symlink(tbuf.as_ptr(), lf.as_ptr()) } == 0 {
                            status = 0;
                            flush();
                        }
                    }
                }
                reply_status(reply, status);
            }
            TAG_FS_READLINK => {
                sync_writes();
                let name = msg_name(&m);
                let mut full = [0u8; 256];
                let mut relbuf = [0u8; PLEN];
                let mut r = MsgBuf::new(0);
                // A symlink target is file content — gate it like TAG_FS_READ so a LIST
                // cap (readdir-only) can't read it.
                if valid && name_ok(name) && allows_read(eff_right(id, cap_right, name)) {
                    if let Some(clen) = join_child(id, name, &mut relbuf) {
                        full_from_rel(&relbuf[..clen], &mut full);
                        let dst = unsafe { (r.data.as_mut_ptr() as *mut u8).add(8) };
                        let mut rcnt = 0usize;
                        if clen > 0
                            && unsafe { oxfs_readlink(full.as_ptr(), dst, 200, &mut rcnt) } == 0
                        {
                            r.data[0] = rcnt as u64; // 0 => error (a real target is non-empty)
                            r.data_len = 64;
                        }
                    }
                }
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_LINK => {
                cache_invalidate();
                sync_writes();
                let win = ((m.data_len as usize) * 8).clamp(64, 512);
                let bytes =
                    unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, win) };
                let slen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let src = &bytes[..slen];
                let dstart = slen + 1;
                let dst = if dstart < win {
                    let rest = &bytes[dstart..];
                    let dl = rest.iter().position(|&b| b == 0).unwrap_or(0);
                    &rest[..dl]
                } else {
                    &bytes[0..0]
                };
                let mut sf = [0u8; 256];
                let mut df = [0u8; 256];
                let mut rb1 = [0u8; PLEN];
                let mut rb2 = [0u8; PLEN];
                let mut status = 1u64;
                // A hard link makes a second name for the SAME inode. Rights here are
                // path-based, so linking a file out of a read-only mount into a writable
                // dir would let it be written through the new name. Require write-class
                // rights on BOTH endpoints, so you can't re-label a read-only inode into a
                // domain that can mutate it.
                if valid && name_ok(src) && name_ok(dst)
                    && allows_link(eff_right(id, cap_right, dst))
                    && allows_link(eff_right(id, cap_right, src))
                {
                    let s = join_child(id, src, &mut rb1);
                    let d = join_child(id, dst, &mut rb2);
                    if let (Some(sl), Some(dl)) = (s, d) {
                        full_from_rel(&rb1[..sl], &mut sf);
                        full_from_rel(&rb2[..dl], &mut df);
                        if sl > 0 && dl > 0 && unsafe { oxfs_link(sf.as_ptr(), df.as_ptr()) } == 0 {
                            status = 0;
                            flush();
                        }
                    }
                }
                reply_status(reply, status);
            }
            _ => reply_status(reply, 1),
        }
    }
}
