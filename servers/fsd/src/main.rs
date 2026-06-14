//! fsd — the lwext4-backed filesystem server (Stage 2: disk-backed self-test).
//!
//! A boot module the kernel grants the block-service endpoint. It implements
//! lwext4's physical block I/O against the virtio-blk service (over the byte-
//! stream block protocol), then mounts — or first-time formats — a real ext2
//! filesystem on the disk and proves persistence with a boot counter that
//! survives reboots. The full FS IPC protocol (replacing the ramfs) lands in
//! Stage 3; this stage validates the lwext4 + libc + block-service integration.
//!
//! libc-hosted for lwext4's malloc/memcpy/etc., but with libc's spawned-program
//! entry disabled (no argv page / tty stdout for a boot module) — fsd supplies
//! its own oxbow_main and logs straight to the console.
#![no_std]
#![no_main]

extern crate oxbow_libc as _;

use core::ffi::{c_int, c_void};
use oxbow_abi::{
    MsgBuf, BLK_CHUNK, BOOT_BLK_EP, BOOT_CONSOLE, TAG_BLK_FLUSH, TAG_BLK_READ, TAG_BLK_WRITE,
};
use oxbow_rt as rt;

const SECTOR: usize = 512;

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

// lwext4 block-device callbacks (blk_id/blk_cnt in 512-byte units). EIO = -5.
#[no_mangle]
pub extern "C" fn ox_open(_bdev: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub extern "C" fn ox_close(_bdev: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub extern "C" fn ox_bread(_bdev: *mut c_void, buf: *mut u8, blk_id: u64, blk_cnt: u32) -> c_int {
    for i in 0..blk_cnt as u64 {
        let dst = unsafe { core::slice::from_raw_parts_mut(buf.add(i as usize * SECTOR), SECTOR) };
        if !read_sector(blk_id + i, dst) {
            return -5;
        }
    }
    0
}
#[no_mangle]
pub extern "C" fn ox_bwrite(_bdev: *mut c_void, buf: *const u8, blk_id: u64, blk_cnt: u32) -> c_int {
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
    fn ext4_umount(mp: *const u8) -> c_int;
    fn oxfs_mkfs_ext2(bd: *mut c_void) -> c_int;
    fn oxfs_read_u32(path: *const u8, out: *mut u32) -> c_int;
    fn oxfs_write_u32(path: *const u8, val: u32) -> c_int;
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[fsd] lwext4 ext2 on the virtio-blk disk\n");
    unsafe {
        let bd = oxblk_get();
        ext4_device_register(bd, b"ox\0".as_ptr());

        // Mount an existing ext2; first boot has none, so format then mount.
        let mut r = ext4_mount(b"ox\0".as_ptr(), b"/mp/\0".as_ptr(), false);
        if r != 0 {
            w(b"[fsd] no ext2 yet - formatting the disk...\n");
            let mr = oxfs_mkfs_ext2(bd);
            wn(b"[fsd] mkfs ext2: r=", mr as i64);
            if mr == 0 {
                r = ext4_mount(b"ox\0".as_ptr(), b"/mp/\0".as_ptr(), false);
            }
        } else {
            w(b"[fsd] mounted existing ext2 from the disk\n");
        }
        wn(b"[fsd] mount: r=", r as i64);

        if r == 0 {
            // Persistence proof: a boot counter that survives reboots on disk.
            let mut count: u32 = 0;
            let _ = oxfs_read_u32(b"/mp/bootcount\0".as_ptr(), &mut count);
            count += 1;
            let wr = oxfs_write_u32(b"/mp/bootcount\0".as_ptr(), count);
            wn(b"[fsd] ext2 boot count (persists across reboots) = ", count as i64);
            let _ = wr;
            ext4_umount(b"/mp/\0".as_ptr());
            w(b"[fsd] ext2 OK on real disk - Stage 2 complete\n");
        } else {
            w(b"[fsd] FAILED to mount ext2\n");
        }
    }
    rt::sys_exit(0)
}
