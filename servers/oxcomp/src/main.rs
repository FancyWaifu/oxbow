//! oxcomp — a tiny Wayland compositor (§42, the graphics climax). A boot server
//! that owns the framebuffer capability and runs real libwayland: it advertises
//! wl_compositor + wl_shm, accepts an in-process client over a socketpair, and on
//! the client's wl_surface.commit composites the client's shm buffer into the
//! framebuffer. So a Wayland client's pixels reach the screen through the entire
//! ported stack (libwayland + libffi + the channel transport + shm/memfd).
//!
//! libc's entry is disabled; we supply oxbow_main, map the framebuffer, then hand
//! a raw pixel pointer to the C compositor driver (comp_main.c).
#![no_std]
#![no_main]
extern crate oxbow_libc as _;

use oxbow_abi::{BOOT_CONSOLE, BOOT_FB, FB_MMIO};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Log helper for the C side (no working stdout in a boot server).
#[no_mangle]
pub extern "C" fn ox_log(p: *const u8, len: usize) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, p, len);
}

extern "C" {
    /// Drive the in-process compositor + client demo, compositing into `fb`
    /// (a 32-bit BGRX framebuffer, `pitch_words` u32 per scanline). Returns 1 if a
    /// client surface was composited.
    fn comp_run(fb: *mut u32, width: i32, height: i32, pitch_words: i32) -> i32;
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let (width, height, pitch, bpp) = match rt::sys_fb_info(BOOT_FB) {
        Ok(g) => g,
        Err(_) => {
            w(b"[oxcomp] no framebuffer capability\n");
            park();
        }
    };
    if bpp != 32 || rt::sys_fb_map(BOOT_FB, FB_MMIO).is_err() {
        w(b"[oxcomp] framebuffer map failed\n");
        park();
    }
    w(b"[oxcomp] compositor up; running client\n");
    let ok = unsafe { comp_run(FB_MMIO as *mut u32, width as i32, height as i32, (pitch / 4) as i32) };
    if ok == 1 {
        w(b"[oxcomp] composited a client surface\n");
    } else {
        w(b"[oxcomp] no surface composited\n");
    }
    park();
}

fn park() -> ! {
    if let Ok(ep) = rt::sys_ep_create() {
        let mut m = oxbow_abi::MsgBuf::new(0);
        loop {
            let _ = rt::sys_recv(ep, &mut m);
        }
    }
    loop {}
}
