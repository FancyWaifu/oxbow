//! Linear framebuffer plumbing. Limine hands us a higher-half-mapped linear
//! framebuffer at boot; we record its geometry + physical base so a userspace
//! `fb` server can later map it as a capability and composite into it. For now
//! the kernel can also paint a test pattern to prove the pipeline end to end.
//!
//! Pixels are assumed 32-bit BGRX (the near-universal Limine default on x86 BIOS
//! + UEFI). `bpp` is checked; non-32bpp framebuffers are ignored (no pixels).

use core::sync::atomic::{AtomicBool, Ordering};

/// Geometry + location of the linear framebuffer.
#[derive(Clone, Copy)]
pub struct FbInfo {
    /// Virtual address Limine mapped it at (in the HHDM / higher half).
    pub virt: u64,
    /// Physical base (for handing the region to a userspace AS as a capability).
    pub phys: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline (>= width * 4; may be padded).
    pub pitch: u32,
    pub bpp: u16,
}

impl FbInfo {
    /// Total mapped size in bytes (pitch * height), rounded up to a page.
    pub fn size_bytes(&self) -> u64 {
        let raw = self.pitch as u64 * self.height as u64;
        (raw + 0xfff) & !0xfff
    }
}

static mut FB: Option<FbInfo> = None;
static READY: AtomicBool = AtomicBool::new(false);

/// Record the framebuffer Limine gave us. `virt` is its mapped address; `phys`
/// is derived from the HHDM offset so the region can be re-mapped elsewhere.
pub fn init(virt: u64, width: u32, height: u32, pitch: u32, bpp: u16) {
    let phys = virt - crate::mm::hhdm_offset();
    unsafe { FB = Some(FbInfo { virt, phys, width, height, pitch, bpp }) };
    READY.store(true, Ordering::Release);
}

/// The recorded framebuffer info, if Limine provided a usable 32bpp one.
pub fn info() -> Option<FbInfo> {
    if READY.load(Ordering::Acquire) {
        unsafe { FB }
    } else {
        None
    }
}

/// Plot one 32-bit BGRX pixel (no-op if out of bounds or no framebuffer).
#[inline]
pub fn put(fb: &FbInfo, x: u32, y: u32, rgb: u32) {
    if x >= fb.width || y >= fb.height {
        return;
    }
    let off = y as u64 * fb.pitch as u64 + x as u64 * 4;
    unsafe { core::ptr::write_volatile((fb.virt + off) as *mut u32, rgb) };
}

/// Paint a diagonal RGB gradient — a quick "the framebuffer works" smoke test,
/// visible in a QEMU screendump before the userspace fb server exists.
pub fn test_pattern() {
    let Some(fb) = info() else { return };
    if fb.bpp != 32 {
        return;
    }
    for y in 0..fb.height {
        for x in 0..fb.width {
            let r = (x * 255 / fb.width) & 0xff;
            let g = (y * 255 / fb.height) & 0xff;
            let b = 0x80u32;
            put(&fb, x, y, (r << 16) | (g << 8) | b);
        }
    }
}
