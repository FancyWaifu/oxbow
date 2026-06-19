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
    FS_DIR, FS_FILE, FS_INITRD, FS_ROOT, PROT_READ, PROT_WRITE, R_GRANT, R_SEND, TAG_BLK_ATTACH,
    TAG_BLK_FLUSH, TAG_BLK_READ, TAG_BLK_READN, TAG_BLK_WRITE, TAG_BLK_WRITEN, TAG_FS_CREATE,
    TAG_FS_MKDIR, TAG_FS_OPEN, TAG_FS_READ, TAG_FS_READDIR, TAG_FS_RENAME, TAG_FS_SYNC,
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

fn cache_invalidate() {
    unsafe {
        C_BLK = [u64::MAX; CWAYS];
        C_PLEN = 0;
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
            oxfs_pread(
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
    fn oxfs_pwrite(path: *const u8, off: u64, buf: *const c_void, len: usize, wr: *mut usize)
        -> c_int;
    fn oxfs_create(path: *const u8) -> c_int;
    fn oxfs_mkdir(path: *const u8) -> c_int;
    fn oxfs_remove(path: *const u8) -> c_int;
    fn oxfs_rename(path: *const u8, new_path: *const u8) -> c_int;
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

fn rel(id: usize) -> &'static [u8] {
    unsafe { &PATHS[id][..PLENS[id] as usize] }
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
    let p = rel(id);
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
        for i in 1..MAXID {
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
    unsafe { oxfs_writeback(1) }; // batch write-back for speed during seeding
    let base = FS_INITRD as *const u8;
    let mut off = 0usize;
    let mut files = 0u32;
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
                    // stream the file body into ext2
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
                    files += 1;
                }
            }
        }
        off += 512 + ((size + 511) & !511);
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
        let id = m.badge as usize;
        let valid = id > 0 && id < MAXID && unsafe { USED[id] };

        match m.tag {
            TAG_FS_OPEN => {
                sync_writes(); // §94: a buffered write must be on disk before stat
                let name = msg_name(&m);
                let mut full = [0u8; 256];
                let mut relbuf = [0u8; PLEN];
                let mut r = MsgBuf::new(0);
                if valid && name_ok(name) {
                    if let Some(clen) = join_child(id, name, &mut relbuf) {
                        if full_from_rel(&relbuf[..clen], &mut full).is_some() {
                            let mut is_dir = 0;
                            let mut size = 0u64;
                            if unsafe { oxfs_stat(full.as_ptr(), &mut is_dir, &mut size) } == 0 {
                                let cid = intern(&relbuf[..clen]);
                                if cid != 0 {
                                    if let Ok(cap) =
                                        rt::sys_mint(BOOT_EP, cid as u64, R_SEND | R_GRANT)
                                    {
                                        r.data[0] = 0;
                                        r.data[1] = if is_dir != 0 { FS_DIR } else { FS_FILE };
                                        r.data[2] = size;
                                        r.data_len = 3;
                                        r.handle_count = 1;
                                        r.handles[0] = cap;
                                        let _ = rt::sys_reply(reply, &r);
                                        let _ = rt::sys_close(cap);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
                reply_status(reply, 1);
            }
            TAG_FS_READ => {
                sync_writes(); // §94: reads must see buffered writes
                let off = m.data[0];
                let mut r = MsgBuf::new(0);
                let mut full = [0u8; 256];
                let mut count = 0usize;
                if valid && full_path(id, &mut full).is_some() {
                    let dst = unsafe { (r.data.as_mut_ptr() as *mut u8).add(8) };
                    // §94: served from the one-block cache — collapses the ~73
                    // path-resolved disk reads per 4 KiB into one.
                    count = unsafe { cached_read(&full, off, dst) };
                }
                r.data[0] = count as u64;
                r.data_len = 64; // all MSG_DATA_WORDS valid (count + up to 504 payload bytes)
                let _ = rt::sys_reply(reply, &r);
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
                if valid && name_ok(name) {
                    let clen = join_child(id, name, &mut relbuf);
                    if let Some(childlen) = clen {
                        full_from_rel(&relbuf[..childlen], &mut full);
                        if childlen > 0 && unsafe { oxfs_create(full.as_ptr()) } == 0 {
                            flush();
                            let cid = intern(&relbuf[..childlen]);
                            if cid != 0 {
                                if let Ok(cap) = rt::sys_mint(BOOT_EP, cid as u64, R_SEND | R_GRANT)
                                {
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
                let count = (m.data[1] as usize).min(48);
                let mut full = [0u8; 256];
                let mut written = 0usize;
                if valid && full_path(id, &mut full).is_some() {
                    let src = unsafe { (m.data.as_ptr() as *const u8).add(16) };
                    // §94: coalesce into 4 KiB blocks instead of open+write+flush
                    // per 48 bytes — the difference between seconds and minutes for
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
                if valid && name_ok(name) {
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
                if valid && name_ok(name) {
                    if let Some(clen) = join_child(id, name, &mut relbuf) {
                        full_from_rel(&relbuf[..clen], &mut full);
                        if clen > 0 && unsafe { oxfs_remove(full.as_ptr()) } == 0 {
                            status = 0;
                            flush();
                        }
                    }
                }
                reply_status(reply, status);
            }
            TAG_FS_RENAME => {
                cache_invalidate(); // §94
                sync_writes();
                // data = old name NUL, then new name NUL.
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let oldlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let old = &bytes[..oldlen];
                let nstart = oldlen + 1;
                let new = if nstart < 64 {
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
                if valid && name_ok(old) && name_ok(new) {
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
            _ => reply_status(reply, 1),
        }
    }
}
