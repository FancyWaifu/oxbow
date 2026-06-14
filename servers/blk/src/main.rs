//! blk — the virtio-blk (modern/MMIO) block driver + sector service.
//!
//! Owns the virtio-blk PCI device the kernel hands it, negotiates modern virtio,
//! and drives a single virtqueue to read/write 512-byte sectors. On top of the
//! raw device it serves a SECTOR read/write endpoint (EP4 / BOOT_EP): the fs
//! server calls it to persist its writable files and restore them at boot.
//!
//! The service keeps a ONE-SECTOR write-back cache: reads/writes name a sector +
//! byte offset, so a client streams arbitrary byte ranges and the driver only
//! touches the disk when the cached sector changes (or on FLUSH). This keeps the
//! 64-byte IPC message a natural unit (<=48 payload bytes) without a disk op per
//! chunk. There is a single disk and a single client, so the endpoint is unbadged.
#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use oxbow_abi::{
    MsgBuf, BLK_CHUNK, BLK_DMA, BLK_MMIO, BLK_SHARED, BLK_XFER_SECTORS, BOOT_CONSOLE, BOOT_EP,
    BOOT_MEM, BOOT_PCI, PROT_READ, PROT_WRITE, TAG_BLK_ATTACH, TAG_BLK_FLUSH, TAG_BLK_READ,
    TAG_BLK_READN, TAG_BLK_WRITE, TAG_BLK_WRITEN,
};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

// --- MMIO register accessors --------------------------------------------------
unsafe fn r8(a: usize) -> u8 {
    read_volatile(a as *const u8)
}
unsafe fn r16(a: usize) -> u16 {
    read_volatile(a as *const u16)
}
unsafe fn w8(a: usize, v: u8) {
    write_volatile(a as *mut u8, v);
}
unsafe fn w16(a: usize, v: u16) {
    write_volatile(a as *mut u16, v);
}
unsafe fn w32(a: usize, v: u32) {
    write_volatile(a as *mut u32, v);
}
unsafe fn w64(a: usize, v: u64) {
    write_volatile(a as *mut u64, v);
}

// --- PCI config-space byte access via the dword sys_pci_read ------------------
fn cfg_dword(off: u32) -> u32 {
    rt::sys_pci_read(BOOT_PCI, off & !3).unwrap_or(0)
}
fn cfg_byte(off: u32) -> u8 {
    (cfg_dword(off) >> ((off & 3) * 8)) as u8
}

// virtio_pci_common_cfg field offsets.
const DEVICE_STATUS: usize = 0x14;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_ENABLE: usize = 0x1C;
const QUEUE_NOTIFY_OFF: usize = 0x1E;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0C;

// device_status bits.
const S_ACK: u8 = 1;
const S_DRIVER: u8 = 2;
const S_DRIVER_OK: u8 = 4;
const S_FEATURES_OK: u8 = 8;

// virtq descriptor flags.
const F_NEXT: u16 = 1;
const F_WRITE: u16 = 2;

// Queue ring offsets within the DMA queue page (separate addresses, modern).
const Q: u16 = 64;
const AVAIL_OFF: usize = 1024;
const USED_OFF: usize = 2048;

const SECTOR: usize = 512;
const NO_SECTOR: u64 = u64::MAX;

struct Dev {
    notify: usize, // notify address for queue 0
    qv: usize,     // queue page vaddr (desc/avail/used rings)
    rv: usize,     // request page vaddr (header @0, data @512, status @1024)
    rp: u64,       // request page phys
}

impl Dev {
    /// The 512-byte sector data buffer (DMA), used as the write-back cache.
    fn data_ptr(&self) -> *mut u8 {
        (self.rv + 512) as *mut u8
    }

    /// Read (write=false) or write (write=true) the sector buffer at `rv+512` to
    /// disk sector `sector`. Returns the virtio-blk status byte (0 = OK).
    unsafe fn op(&self, sector: u64, write: bool) -> u8 {
        let hdr = self.rv;
        let stat = self.rv + 1024;
        let hdr_p = self.rp;
        let dbuf_p = self.rp + 512;
        let stat_p = self.rp + 1024;

        // virtio_blk_req header: type (0=read,1=write), reserved, sector.
        w32(hdr, if write { 1 } else { 0 });
        w32(hdr + 4, 0);
        w64(hdr + 8, sector);

        // Three descriptors at qv: header (RO) -> data -> status (device-write).
        w64(self.qv, hdr_p);
        w32(self.qv + 8, 16);
        w16(self.qv + 12, F_NEXT);
        w16(self.qv + 14, 1);

        let dflags = F_NEXT | if write { 0 } else { F_WRITE };
        w64(self.qv + 16, dbuf_p);
        w32(self.qv + 24, SECTOR as u32);
        w16(self.qv + 28, dflags);
        w16(self.qv + 30, 2);

        w64(self.qv + 32, stat_p);
        w32(self.qv + 40, 1);
        w16(self.qv + 44, F_WRITE);
        w16(self.qv + 46, 0);

        // avail ring: flags(u16) idx(u16) ring[](u16). Offer descriptor head 0.
        let avail = self.qv + AVAIL_OFF;
        let aidx = r16(avail + 2);
        w16(avail + 4 + (aidx % Q) as usize * 2, 0);
        fence(Ordering::SeqCst);
        w16(avail + 2, aidx.wrapping_add(1));
        fence(Ordering::SeqCst);

        // Notify the device of queue 0, then poll the used ring for completion.
        w16(self.notify, 0);
        let used = self.qv + USED_OFF;
        let start = r16(used + 2);
        let mut spins: u64 = 0;
        while r16(used + 2) == start {
            spins += 1;
            if spins > 1_000_000_000 {
                return 0xff;
            }
        }
        fence(Ordering::SeqCst);
        r8(stat)
    }
}

/// One-sector write-back cache over the device.
struct Cache {
    dev: Dev,
    cached: u64, // sector currently in the buffer, or NO_SECTOR
    dirty: bool,
}

impl Cache {
    /// Make `sector` the cached sector, flushing a dirty different one first and
    /// reading the new one in (so partial writes preserve the rest). Returns false
    /// on a disk error.
    unsafe fn ensure(&mut self, sector: u64) -> bool {
        if self.cached == sector {
            return true;
        }
        if self.dirty && self.cached != NO_SECTOR && self.dev.op(self.cached, true) != 0 {
            return false;
        }
        self.dirty = false;
        if self.dev.op(sector, false) != 0 {
            self.cached = NO_SECTOR;
            return false;
        }
        self.cached = sector;
        true
    }

    /// Commit the cached sector if dirty.
    unsafe fn flush(&mut self) -> bool {
        if self.dirty && self.cached != NO_SECTOR {
            if self.dev.op(self.cached, true) != 0 {
                return false;
            }
            self.dirty = false;
        }
        true
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[blk] virtio-blk init\n");
    let cmd = cfg_dword(0x04);
    let _ = rt::sys_pci_write(BOOT_PCI, 0x04, cmd | 0x6);

    // Walk the PCI capability list for the virtio common/notify caps.
    let mut cap = (cfg_byte(0x34) & 0xFC) as u32;
    let mut common_bar = 0u8;
    let mut common_off = 0u32;
    let mut notify_bar = 0u8;
    let mut notify_off = 0u32;
    let mut notify_mult = 0u32;
    let mut guard = 0;
    let mut have_dev = cfg_dword(0x00) & 0xFFFF == 0x1af4;
    while cap != 0 && guard < 32 {
        guard += 1;
        let d0 = cfg_dword(cap);
        let cap_id = (d0 & 0xFF) as u8;
        let cap_next = ((d0 >> 8) & 0xFF) as u8;
        let cfg_type = ((d0 >> 24) & 0xFF) as u8;
        if cap_id == 0x09 {
            let bar = (cfg_dword(cap + 4) & 0xFF) as u8;
            let offset = cfg_dword(cap + 8);
            match cfg_type {
                1 => {
                    common_bar = bar;
                    common_off = offset;
                }
                2 => {
                    notify_bar = bar;
                    notify_off = offset;
                    notify_mult = cfg_dword(cap + 16);
                }
                _ => {}
            }
        }
        cap = (cap_next & 0xFC) as u32;
    }
    if common_bar != notify_bar {
        have_dev = false;
    }

    // Map the BAR + bring the device up. Any failure drops to a degraded service
    // that fails every request, so the fs server gets a clean error (and skips
    // restore) instead of blocking on a never-ready disk.
    let mut cache: Option<Cache> = None;
    if have_dev && rt::sys_pci_bar_map(BOOT_PCI, common_bar as u32, BLK_MMIO).is_ok() {
        let cc = BLK_MMIO as usize + common_off as usize;
        unsafe {
            w8(cc + DEVICE_STATUS, 0);
            while r8(cc + DEVICE_STATUS) != 0 {}
            w8(cc + DEVICE_STATUS, S_ACK);
            w8(cc + DEVICE_STATUS, S_ACK | S_DRIVER);
            // Accept only VIRTIO_F_VERSION_1 (feature bit 32).
            w32(cc + DRIVER_FEATURE_SELECT, 1);
            w32(cc + DRIVER_FEATURE, 1);
            w32(cc + DRIVER_FEATURE_SELECT, 0);
            w32(cc + DRIVER_FEATURE, 0);
            w8(cc + DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
            if r8(cc + DEVICE_STATUS) & S_FEATURES_OK != 0 {
                w16(cc + QUEUE_SELECT, 0);
                let maxq = r16(cc + QUEUE_SIZE);
                let q = if maxq < Q { maxq } else { Q };
                w16(cc + QUEUE_SIZE, q);
                let qp = rt::sys_dma_alloc(BOOT_MEM, BLK_DMA).unwrap_or(0);
                let rp = rt::sys_dma_alloc(BOOT_MEM, BLK_DMA + 0x1000).unwrap_or(0);
                if qp != 0 && rp != 0 {
                    w64(cc + QUEUE_DESC, qp);
                    w64(cc + QUEUE_DRIVER, qp + AVAIL_OFF as u64);
                    w64(cc + QUEUE_DEVICE, qp + USED_OFF as u64);
                    w16(cc + QUEUE_ENABLE, 1);
                    w8(cc + DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);
                    let qnoff = r16(cc + QUEUE_NOTIFY_OFF);
                    let notify = BLK_MMIO as usize
                        + notify_off as usize
                        + qnoff as usize * notify_mult as usize;
                    cache = Some(Cache {
                        dev: Dev { notify, qv: BLK_DMA as usize, rv: (BLK_DMA + 0x1000) as usize, rp },
                        cached: NO_SECTOR,
                        dirty: false,
                    });
                }
            }
        }
    }

    match &cache {
        Some(_) => w(b"[blk] sector service ready\n"),
        None => w(b"[blk] no disk - degraded (requests fail)\n"),
    }

    // `shared` is the vaddr of the client's transfer frame (TAG_BLK_ATTACH),
    // mapped here once for fast whole-sector copies (bulk READN/WRITEN).
    let mut shared: Option<usize> = None;

    // Service loop: byte-stream sector ops (legacy) + bulk shared-frame transfers.
    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_EP, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut r = MsgBuf::new(0);
        match m.tag {
            TAG_BLK_ATTACH => {
                // Map the client's shared transfer frame (handles[0]) read+write.
                let mut status = 1u64;
                if m.handle_count >= 1 {
                    if rt::sys_frame_map(m.handles[0], BLK_SHARED, PROT_READ | PROT_WRITE).is_ok() {
                        shared = Some(BLK_SHARED as usize);
                        status = 0;
                    }
                }
                r.data[0] = status;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_BLK_READN => {
                let sector = m.data[0];
                let n = (m.data[1]).min(BLK_XFER_SECTORS);
                let mut status = 1u64;
                if let (Some(sh), Some(c)) = (shared, cache.as_mut()) {
                    let _ = unsafe { c.flush() }; // coherency vs the byte-stream cache
                    status = 0;
                    for i in 0..n {
                        if unsafe { c.dev.op(sector + i, false) } != 0 {
                            status = 1;
                            break;
                        }
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                c.dev.data_ptr(),
                                (sh + i as usize * SECTOR) as *mut u8,
                                SECTOR,
                            );
                        }
                    }
                    c.cached = NO_SECTOR; // dev.op clobbered the cache buffer
                }
                r.data[0] = status;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_BLK_WRITEN => {
                let sector = m.data[0];
                let n = (m.data[1]).min(BLK_XFER_SECTORS);
                let mut status = 1u64;
                if let (Some(sh), Some(c)) = (shared, cache.as_mut()) {
                    let _ = unsafe { c.flush() };
                    status = 0;
                    for i in 0..n {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (sh + i as usize * SECTOR) as *const u8,
                                c.dev.data_ptr(),
                                SECTOR,
                            );
                        }
                        if unsafe { c.dev.op(sector + i, true) } != 0 {
                            status = 1;
                            break;
                        }
                    }
                    c.cached = NO_SECTOR;
                }
                r.data[0] = status;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_BLK_READ => {
                let sector = m.data[0];
                let off = (m.data[1] as usize).min(SECTOR);
                let mut count = (SECTOR - off).min(BLK_CHUNK);
                let mut ok = false;
                if let Some(c) = cache.as_mut() {
                    if unsafe { c.ensure(sector) } {
                        let src = unsafe { c.dev.data_ptr().add(off) };
                        let dst = unsafe { (r.data.as_mut_ptr() as *mut u8).add(8) };
                        unsafe { core::ptr::copy_nonoverlapping(src, dst, count) };
                        ok = true;
                    }
                }
                if !ok {
                    count = 0;
                }
                r.data[0] = count as u64;
                r.data_len = 8;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_BLK_WRITE => {
                let sector = m.data[0];
                let off = (m.data[1] as usize).min(SECTOR);
                let count = (m.data[2] as usize).min(BLK_CHUNK).min(SECTOR - off);
                let mut written = 0usize;
                if let Some(c) = cache.as_mut() {
                    if unsafe { c.ensure(sector) } {
                        let src = unsafe { (m.data.as_ptr() as *const u8).add(24) };
                        let dst = unsafe { c.dev.data_ptr().add(off) };
                        unsafe { core::ptr::copy_nonoverlapping(src, dst, count) };
                        c.dirty = true;
                        written = count;
                    }
                }
                r.data[0] = written as u64;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_BLK_FLUSH => {
                let ok = cache.as_mut().map(|c| unsafe { c.flush() }).unwrap_or(false);
                r.data[0] = if ok { 0 } else { 1 };
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            _ => {
                r.data[0] = 1;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
        }
    }
}
