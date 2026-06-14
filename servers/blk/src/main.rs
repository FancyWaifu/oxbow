//! blk — the virtio-blk (modern/MMIO) block driver. Owns the virtio-blk PCI
//! device, negotiates features, sets up a single virtqueue in DMA memory, and
//! reads/writes 512-byte sectors. Stage 1: a self-test (read sector 0, write a
//! pattern to sector 1, read it back) proving the disk works; the block-service
//! IPC + fs persistence layer comes next.
#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use oxbow_abi::{BLK_DMA, BLK_MMIO, BOOT_CONSOLE, BOOT_MEM, BOOT_PCI};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}
fn wn(label: &[u8], n: u64) {
    w(label);
    let mut b = [0u8; 20];
    let mut i = 20;
    let mut v = n;
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    w(&b[i..]);
    w(b"\n");
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

struct Blk {
    common: usize, // common cfg MMIO base
    notify: usize, // notify address for queue 0
    qv: usize,     // queue page vaddr
    rv: usize,     // request page vaddr
    rp: u64,       // request page phys
}

impl Blk {
    /// Read (write=false) or write (write=true) one 512-byte sector. For a write,
    /// `buf` is the data to write; for a read, the sector lands in `buf`. Returns
    /// the virtio-blk status byte (0 = OK).
    unsafe fn op(&self, sector: u64, write: bool, buf: &mut [u8; 512]) -> u8 {
        let hdr = self.rv;
        let dbuf = self.rv + 512;
        let stat = self.rv + 1024;
        let hdr_p = self.rp;
        let dbuf_p = self.rp + 512;
        let stat_p = self.rp + 1024;

        // virtio_blk_req header: type (0=read,1=write), reserved, sector.
        w32(hdr, if write { 1 } else { 0 });
        w32(hdr + 4, 0);
        w64(hdr + 8, sector);
        if write {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), dbuf as *mut u8, 512);
        }

        // Three descriptors at qv: header (RO) -> data -> status (device-write).
        // desc i: addr(u64) len(u32) flags(u16) next(u16) = 16 bytes.
        w64(self.qv, hdr_p);
        w32(self.qv + 8, 16);
        w16(self.qv + 12, F_NEXT);
        w16(self.qv + 14, 1);

        let dflags = F_NEXT | if write { 0 } else { F_WRITE };
        w64(self.qv + 16, dbuf_p);
        w32(self.qv + 24, 512);
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

        // Notify the device of queue 0.
        w16(self.notify, 0);

        // Poll the used ring for completion.
        let used = self.qv + USED_OFF;
        let start = r16(used + 2);
        let mut spins: u64 = 0;
        while r16(used + 2) == start {
            spins += 1;
            if spins > 500_000_000 {
                w(b"[blk] request timed out\n");
                return 0xff;
            }
        }
        fence(Ordering::SeqCst);
        if !write {
            core::ptr::copy_nonoverlapping(dbuf as *const u8, buf.as_mut_ptr(), 512);
        }
        r8(stat)
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // 1. Confirm the device; enable mem-space + bus master.
    let id = cfg_dword(0x00);
    w(b"[blk] virtio-blk init\n");
    let cmd = cfg_dword(0x04);
    let _ = rt::sys_pci_write(BOOT_PCI, 0x04, cmd | 0x6);
    let _ = id;

    // 2. Walk the PCI capability list for the virtio common/notify/device caps.
    let mut cap = (cfg_byte(0x34) & 0xFC) as u32;
    let mut common_bar = 0u8;
    let mut common_off = 0u32;
    let mut notify_bar = 0u8;
    let mut notify_off = 0u32;
    let mut notify_mult = 0u32;
    let mut guard = 0;
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
        w(b"[blk] caps span multiple BARs (unsupported)\n");
        rt::sys_exit(1);
    }

    // 3. Map the device BAR.
    if rt::sys_pci_bar_map(BOOT_PCI, common_bar as u32, BLK_MMIO).is_err() {
        w(b"[blk] BAR map FAILED\n");
        rt::sys_exit(1);
    }
    let cc = BLK_MMIO as usize + common_off as usize;

    // 4. Reset, negotiate (VIRTIO_F_VERSION_1, feature bit 32), set up queue 0.
    let rp;
    let blk;
    unsafe {
        w8(cc + DEVICE_STATUS, 0);
        while r8(cc + DEVICE_STATUS) != 0 {}
        w8(cc + DEVICE_STATUS, S_ACK);
        w8(cc + DEVICE_STATUS, S_ACK | S_DRIVER);
        // accept only VERSION_1 (feature 32 = bit 0 of the high feature word).
        w32(cc + DRIVER_FEATURE_SELECT, 1);
        w32(cc + DRIVER_FEATURE, 1);
        w32(cc + DRIVER_FEATURE_SELECT, 0);
        w32(cc + DRIVER_FEATURE, 0);
        w8(cc + DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
        if r8(cc + DEVICE_STATUS) & S_FEATURES_OK == 0 {
            w(b"[blk] FEATURES_OK rejected\n");
            rt::sys_exit(1);
        }
        w16(cc + QUEUE_SELECT, 0);
        let maxq = r16(cc + QUEUE_SIZE);
        let q = if maxq < Q { maxq } else { Q };
        w16(cc + QUEUE_SIZE, q);

        let qp = rt::sys_dma_alloc(BOOT_MEM, BLK_DMA).expect("[blk] dma q");
        rp = rt::sys_dma_alloc(BOOT_MEM, BLK_DMA + 0x1000).expect("[blk] dma r");
        w64(cc + QUEUE_DESC, qp);
        w64(cc + QUEUE_DRIVER, qp + AVAIL_OFF as u64);
        w64(cc + QUEUE_DEVICE, qp + USED_OFF as u64);
        w16(cc + QUEUE_ENABLE, 1);
        w8(cc + DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);

        let qnoff = r16(cc + QUEUE_NOTIFY_OFF);
        let notify = BLK_MMIO as usize + notify_off as usize + qnoff as usize * notify_mult as usize;

        blk = Blk {
            common: cc,
            notify,
            qv: BLK_DMA as usize,
            rv: (BLK_DMA + 0x1000) as usize,
            rp,
        };
    }
    let _ = blk.common;
    w(b"[blk] queue ready, running self-test\n");

    // 5. Self-test: write a known pattern to sector 1, read it back, verify; and
    //    read sector 0 (a freshly created image is zeros).
    unsafe {
        let mut buf = [0u8; 512];
        // read sector 0
        let st = blk.op(0, false, &mut buf);
        wn(b"[blk] read sector 0 status=", st as u64);
        wn(b"[blk]   sector0[0..4] sum=", buf[0..4].iter().map(|&b| b as u64).sum());

        // write a pattern to sector 1
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i as u8) ^ 0x5a;
        }
        let st = blk.op(1, true, &mut buf);
        wn(b"[blk] write sector 1 status=", st as u64);

        // read it back into a fresh buffer + verify
        let mut rd = [0u8; 512];
        let st = blk.op(1, false, &mut rd);
        wn(b"[blk] read-back status=", st as u64);
        let mut ok = true;
        for i in 0..512 {
            if rd[i] != ((i as u8) ^ 0x5a) {
                ok = false;
                break;
            }
        }
        if ok {
            w(b"[blk] SELF-TEST PASS: wrote + read back 512 bytes to disk!\n");
        } else {
            w(b"[blk] SELF-TEST FAIL: read-back mismatch\n");
        }
    }

    rt::sys_exit(0)
}
