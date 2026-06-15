//! fb — the framebuffer server: the sole holder of the framebuffer capability
//! and thus the only process that can put a pixel on screen. The kernel grants
//! it BOOT_FB at boot; it maps the linear framebuffer (SYS_FB_MAP) and queries
//! geometry (SYS_FB_INFO). This is the foundation of the window server: clients
//! will later send draw/compose requests over IPC and never touch the pixels
//! directly (zero ambient authority — every GUI pixel flows through here).
//!
//! This first cut composites a static "desktop" — a background plus a couple of
//! framed windows with title bars — to prove the userspace drawing path end to
//! end. Compositing of real client surfaces over IPC comes next.
#![no_std]
#![no_main]

use core::ptr::write_volatile;
use oxbow_abi::{MsgBuf, BOOT_CONSOLE, BOOT_FB, FB_MMIO};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Park forever without burning the CPU: block on a receive from a private
/// endpoint that has no other sender. (The compositor IPC loop replaces this.)
fn park() -> ! {
    if let Ok(ep) = rt::sys_ep_create() {
        let mut m = MsgBuf::new(0);
        loop {
            let _ = rt::sys_recv(ep, &mut m);
        }
    }
    loop {}
}

/// A mapped linear framebuffer we can draw into (32-bit BGRX).
struct Fb {
    base: u64,
    width: u32,
    height: u32,
    pitch: u32,
}

impl Fb {
    #[inline]
    fn put(&self, x: u32, y: u32, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let off = y as u64 * self.pitch as u64 + x as u64 * 4;
        unsafe { write_volatile((self.base + off) as *mut u32, color) };
    }

    /// Fill the rectangle [x,x+w) × [y,y+h), clipped to the screen.
    fn fill_rect(&self, x: u32, y: u32, rw: u32, rh: u32, color: u32) {
        let x1 = (x + rw).min(self.width);
        let y1 = (y + rh).min(self.height);
        let mut yy = y;
        while yy < y1 {
            let mut xx = x;
            while xx < x1 {
                self.put(xx, yy, color);
                xx += 1;
            }
            yy += 1;
        }
    }

    fn clear(&self, color: u32) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// A window: a colored title bar atop a body, with a 1px frame.
    fn window(&self, x: u32, y: u32, rw: u32, rh: u32, title_color: u32, body_color: u32) {
        let frame = 0x20_2020;
        self.fill_rect(x, y, rw, rh, frame); // frame/outline
        self.fill_rect(x + 1, y + 1, rw - 2, 22, title_color); // title bar
        self.fill_rect(x + 1, y + 23, rw - 2, rh - 24, body_color); // body
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // Geometry + map the framebuffer at the fixed vaddr.
    let (width, height, pitch, bpp) = match rt::sys_fb_info(BOOT_FB) {
        Ok(g) => g,
        Err(_) => {
            w(b"[fb] no framebuffer capability\n");
            park();
        }
    };
    if bpp != 32 || rt::sys_fb_map(BOOT_FB, FB_MMIO).is_err() {
        w(b"[fb] map failed or unsupported bpp\n");
        park();
    }
    let fb = Fb { base: FB_MMIO, width, height, pitch };

    // Compose a simple desktop to prove the userspace drawing path.
    fb.clear(0x0d_3b45); // deep teal background
    fb.fill_rect(0, height - 28, width, 28, 0x1a_1a24); // bottom "panel"
    fb.window(80, 80, 360, 240, 0x2d_6cdf, 0xf0_f0f5); // window 1 (blue title)
    fb.window(520, 200, 420, 300, 0xb0_3a4a, 0x10_1018); // window 2 (red title, dark body)
    // A little color swatch row on the panel.
    let mut sx = 8;
    for c in [0xe0_4040u32, 0xe0_a040, 0x40_c040, 0x4080e0, 0xa050d0] {
        fb.fill_rect(sx, height - 22, 16, 16, c);
        sx += 22;
    }

    w(b"[fb] desktop composited\n");
    park();
}
