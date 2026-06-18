//! Minimal modern-virtio (1.0+) PCI transport — the reusable plumbing a virtio
//! driver needs, factored out of the device logic. Covers: walking the PCI
//! capability list for the virtio cfg structures (common / notify / isr / device),
//! mapping the BAR, the device-status + feature-negotiation handshake, and a
//! virtqueue with synchronous request/response submission.
//!
//! This is the same transport `servers/blk` implements inline; the gpu driver
//! gets it as a module (a future cleanup migrates blk onto it too). Submission is
//! poll-based: virtio-gpu commands are synchronous request→response, so we offer a
//! descriptor chain, notify, and spin the used ring. (Async events — config-change
//! / display hotplug — will use the IRQ path in a later phase.)

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use oxbow_abi::{Handle, BOOT_CONSOLE};
use oxbow_rt as rt;

fn dbg(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}
fn dbgn(mut n: u32) {
    let mut b = [0u8; 10];
    let mut i = 10;
    loop {
        i -= 1;
        b[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    dbg(&b[i..]);
}

// --- raw MMIO accessors -------------------------------------------------------
#[inline]
pub unsafe fn r8(a: usize) -> u8 {
    read_volatile(a as *const u8)
}
#[inline]
pub unsafe fn r16(a: usize) -> u16 {
    read_volatile(a as *const u16)
}
#[inline]
pub unsafe fn r32(a: usize) -> u32 {
    read_volatile(a as *const u32)
}
#[inline]
pub unsafe fn w8(a: usize, v: u8) {
    write_volatile(a as *mut u8, v);
}
#[inline]
pub unsafe fn w16(a: usize, v: u16) {
    write_volatile(a as *mut u16, v);
}
#[inline]
pub unsafe fn w32(a: usize, v: u32) {
    write_volatile(a as *mut u32, v);
}
#[inline]
pub unsafe fn w64(a: usize, v: u64) {
    write_volatile(a as *mut u64, v);
}

// --- virtio_pci_common_cfg field offsets --------------------------------------
const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0C;
const DEVICE_STATUS: usize = 0x14;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_ENABLE: usize = 0x1C;
const QUEUE_NOTIFY_OFF: usize = 0x1E;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;

// device_status bits.
const S_ACK: u8 = 1;
const S_DRIVER: u8 = 2;
const S_DRIVER_OK: u8 = 4;
const S_FEATURES_OK: u8 = 8;

// virtq descriptor flags.
const F_NEXT: u16 = 1;
const F_WRITE: u16 = 2;

/// Queue ring layout in one DMA page: desc table (Q*16) at 0, avail ring at
/// AVAIL_OFF, used ring at USED_OFF. Sized for Q<=64 (the cap we request), all
/// fitting under 4 KiB (2048 + 4 + 64*8 = 2564).
pub const Q: u16 = 64;
const AVAIL_OFF: usize = 1024;
const USED_OFF: usize = 2048;

/// The located virtio configuration structures, all expressed as vaddrs into the
/// single mapped BAR. (QEMU places every virtio cap in one BAR; we require that.)
pub struct Transport {
    pub common: usize,      // virtio_pci_common_cfg
    pub notify_base: usize, // notify BAR region base
    pub notify_mult: u32,   // notify_off_multiplier
    pub isr: usize,         // ISR status byte
    pub device: usize,      // device-specific config (e.g. virtio_gpu_config)
}

/// Read a PCI config dword (the syscall is dword-granular).
fn cfg_dword(pci: Handle, off: u32) -> u32 {
    rt::sys_pci_read(pci, off & !3).unwrap_or(0)
}
fn cfg_byte(pci: Handle, off: u32) -> u8 {
    (cfg_dword(pci, off) >> ((off & 3) * 8)) as u8
}

impl Transport {
    /// Discover the virtio cfg caps on `pci`, map the (single) BAR they live in at
    /// `bar_vaddr`, and return the cfg structure vaddrs. Enables MMIO + bus-master
    /// in the command register first. None if the device isn't a single-BAR modern
    /// virtio device.
    pub fn probe(pci: Handle, bar_vaddr: u64) -> Option<Transport> {
        // Enable memory space + bus mastering (DMA).
        let cmd = cfg_dword(pci, 0x04);
        let _ = rt::sys_pci_write(pci, 0x04, cmd | 0x6);

        // Record each cfg type's (bar, offset) independently — DON'T lock onto the
        // first cap's bar (a VIRTIO_PCI_CAP_PCI_CFG, type 5, carries bar=0 and may
        // appear first). Anchor on the common cfg's bar afterward.
        let mut bars = [-1i32; 5]; // index by cfg_type 1..=4
        let mut offs = [0u32; 5];
        let mut notify_mult = 0u32;
        let mut cap = (cfg_byte(pci, 0x34) & 0xFC) as u32;
        let mut guard = 0;
        while cap != 0 && guard < 48 {
            guard += 1;
            let d0 = cfg_dword(pci, cap);
            let cap_id = (d0 & 0xFF) as u8;
            let cap_next = ((d0 >> 8) & 0xFF) as u8;
            let cfg_type = ((d0 >> 24) & 0xFF) as usize;
            if cap_id == 0x09 && (1..=4).contains(&cfg_type) {
                bars[cfg_type] = (cfg_dword(pci, cap + 4) & 0xFF) as i32;
                offs[cfg_type] = cfg_dword(pci, cap + 8);
                if cfg_type == 2 {
                    notify_mult = cfg_dword(pci, cap + 16);
                }
            }
            cap = (cap_next & 0xFC) as u32;
        }
        let bar = bars[1]; // the common cfg's bar anchors the mapping
        if bar < 0 || bars[2] < 0 || bars[3] < 0 || bars[4] < 0 {
            dbg(b"[gpu] missing a virtio cfg cap\n");
            return None;
        }
        if bars[2] != bar || bars[3] != bar || bars[4] != bar {
            dbg(b"[gpu] cfg caps span multiple BARs (unsupported)\n");
            return None;
        }
        if rt::sys_pci_bar_map(pci, bar as u32, bar_vaddr).is_err() {
            dbg(b"[gpu] BAR map failed bar=");
            dbgn(bar as u32);
            dbg(b"\n");
            return None;
        }
        let base = bar_vaddr as usize;
        Some(Transport {
            common: base + offs[1] as usize,
            notify_base: base + offs[2] as usize,
            notify_mult,
            isr: base + offs[3] as usize,
            device: base + offs[4] as usize,
        })
    }

    /// Reset, ACK, DRIVER, negotiate features, FEATURES_OK. `driver_features` is
    /// the low 64 feature bits we accept (must include VIRTIO_F_VERSION_1, bit 32).
    /// Returns false if the device rejects our feature set. DRIVER_OK is set later
    /// by `finish` once the queues are configured.
    pub unsafe fn begin(&self, driver_features: u64) -> bool {
        let c = self.common;
        w8(c + DEVICE_STATUS, 0);
        while r8(c + DEVICE_STATUS) != 0 {}
        w8(c + DEVICE_STATUS, S_ACK);
        w8(c + DEVICE_STATUS, S_ACK | S_DRIVER);
        // Read what the device offers (low + high dwords), AND with what we want.
        w32(c + DEVICE_FEATURE_SELECT, 0);
        let dev_lo = r32(c + DEVICE_FEATURE) as u64;
        w32(c + DEVICE_FEATURE_SELECT, 1);
        let dev_hi = r32(c + DEVICE_FEATURE) as u64;
        let offered = dev_lo | (dev_hi << 32);
        let want = offered & driver_features;
        w32(c + DRIVER_FEATURE_SELECT, 0);
        w32(c + DRIVER_FEATURE, want as u32);
        w32(c + DRIVER_FEATURE_SELECT, 1);
        w32(c + DRIVER_FEATURE, (want >> 32) as u32);
        w8(c + DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
        r8(c + DEVICE_STATUS) & S_FEATURES_OK != 0
    }

    /// Set DRIVER_OK — the device is live (call after queues are set up).
    pub unsafe fn finish(&self) {
        w8(self.common + DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);
    }

    /// Configure virtqueue `idx` whose ring page is at (`ring_vaddr`,`ring_phys`),
    /// and return a `Vq` handle for submission. Selects the queue, clamps its size
    /// to Q, points the device at our ring addresses, enables it, and computes the
    /// notify address.
    pub unsafe fn setup_queue(&self, idx: u16, ring_vaddr: usize, ring_phys: u64) -> Vq {
        let c = self.common;
        w16(c + QUEUE_SELECT, idx);
        let maxq = r16(c + QUEUE_SIZE);
        let size = if maxq == 0 || maxq > Q { Q } else { maxq };
        w16(c + QUEUE_SIZE, size);
        w64(c + QUEUE_DESC, ring_phys);
        w64(c + QUEUE_DRIVER, ring_phys + AVAIL_OFF as u64);
        w64(c + QUEUE_DEVICE, ring_phys + USED_OFF as u64);
        w16(c + QUEUE_ENABLE, 1);
        let qnoff = r16(c + QUEUE_NOTIFY_OFF);
        let notify = self.notify_base + qnoff as usize * self.notify_mult as usize;
        Vq { qv: ring_vaddr, size, notify, last_used: 0 }
    }
}

/// A configured virtqueue, ready for synchronous submission.
pub struct Vq {
    qv: usize,       // ring page vaddr
    size: u16,       // negotiated queue size
    notify: usize,   // device notify address for this queue
    last_used: u16,  // last used-ring index we consumed
}

impl Vq {
    /// Submit a request→response pair: descriptor 0 = `req` bytes (device reads),
    /// descriptor 1 = `resp` bytes (device writes). Both are physical addresses of
    /// DMA buffers. Notifies the device and spins the used ring until completion.
    /// Returns false on timeout. Caller reads the response from its DMA buffer.
    pub unsafe fn request(&mut self, req_p: u64, req_len: u32, resp_p: u64, resp_len: u32) -> bool {
        let d = self.qv; // descriptor table at offset 0
        // desc[0]: request (device-read), chained to desc[1].
        w64(d, req_p);
        w32(d + 8, req_len);
        w16(d + 12, F_NEXT);
        w16(d + 14, 1);
        // desc[1]: response (device-write).
        w64(d + 16, resp_p);
        w32(d + 24, resp_len);
        w16(d + 28, F_WRITE);
        w16(d + 30, 0);

        let avail = self.qv + AVAIL_OFF;
        let aidx = r16(avail + 2);
        w16(avail + 4 + (aidx % self.size) as usize * 2, 0); // offer descriptor head 0
        fence(Ordering::SeqCst);
        w16(avail + 2, aidx.wrapping_add(1));
        fence(Ordering::SeqCst);

        w16(self.notify, 0); // kick queue (the value is the queue index; index 0 here)
        let used = self.qv + USED_OFF;
        let mut spins: u64 = 0;
        while r16(used + 2) == self.last_used {
            spins += 1;
            if spins > 2_000_000_000 {
                return false;
            }
            core::hint::spin_loop();
        }
        self.last_used = self.last_used.wrapping_add(1);
        fence(Ordering::SeqCst);
        true
    }
}
