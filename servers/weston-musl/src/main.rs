//! weston — the real upstream Weston compositor (libweston) ported to oxbow, replacing
//! oxcomp. Software path only: pixman renderer + a custom fbdev-style backend pointed at
//! oxbow's framebuffer, input injected from the oxbow kbd/mouse channels, clients attached
//! via the inherited-fd model (wl_client_create on channel fds). See docs/weston-port.md.
//!
//! Musl-personality app: all the C (libweston + pixman + libwayland + xkbcommon + the
//! personality bridge) + musl libc.a are compiled/linked by build.rs; the C `main` (a P1
//! stub in glue.c, real compositor main in P2) is the entry via crt_glue. The Rust side is
//! just the allocator + panic handler + oxbow-rt[hosted] `_start` glue (like havoc/Xwayland).
#![no_std]
#![no_main]

extern crate oxbow_rt as _;

use core::alloc::{GlobalAlloc, Layout};
use oxbow_rt as rt;

extern "C" {
    fn malloc(n: usize) -> *mut u8;
    fn free(p: *mut u8);
    fn __oxbow_exit(code: i32) -> !;
}

/// P4/P5: spawn a Wayland client (app_id: 0 = wlclient "rings", 1 = havoc terminal, 2 =
/// oxterm) handing it a fresh channel end as its Wayland socket (slot 4), and return the
/// compositor's end wrapped as an fd for wl_client_create — the inherited-fd model oxcomp
/// uses. Returns -1 on failure.
#[no_mangle]
pub extern "C" fn oxbow_spawn_wl_client(app_id: i32) -> i32 {
    use oxbow_abi::{
        BOOT_CONSOLE, BOOT_FS_ROOT, BOOT_IMG_HAVOC, BOOT_IMG_OXTERM, BOOT_IMG_WLCLIENT, BOOT_MEM,
        BOOT_TERM_CHAN, HANDLE_NULL, Handle, MsgBuf,
    };
    extern "C" {
        fn ox_chan_fd(slot: i32) -> i32; /* personality: wrap a channel cap as an fd */
    }
    let img: Handle = match app_id {
        1 => BOOT_IMG_HAVOC,
        2 => BOOT_IMG_OXTERM,
        _ => BOOT_IMG_WLCLIENT,
    };
    let Some((srv, cli)) = rt::channel::pair() else {
        return -1;
    };
    let mut m = MsgBuf::new(0);
    m.data[0] = 32 * 1024 * 1024; // working set + a full-screen buffer
    m.data_len = 3;
    m.handle_count = 4;
    m.handles[0] = BOOT_FS_ROOT; // slot 1: fs (ld-oxbow's /lib, /bin/sh)
    m.handles[1] = BOOT_CONSOLE; // slot 2: console
    m.handles[2] = cli; // slot 4: the Wayland socket
    m.handles[3] = if app_id == 2 { BOOT_TERM_CHAN } else { HANDLE_NULL }; // slot 20: oxterm tty
    if rt::sys_spawn(img, BOOT_MEM, &m, HANDLE_NULL).is_ok() {
        unsafe { ox_chan_fd(srv as i32) }
    } else {
        let _ = rt::sys_close(srv);
        let _ = rt::sys_close(cli);
        -1
    }
}

/// Native shim for the C frontend: map the GPU shared framebuffer (BOOT_GPU_FB) at
/// FB_MMIO — the same buffer the gpu driver scans out — and report its geometry. The C
/// side's pixman renderer then draws straight into it. Returns NULL if the fb cap isn't
/// held (weston must be spawned with BOOT_GPU_FB granted). Mirrors oxcomp's fb bring-up.
#[no_mangle]
pub extern "C" fn oxbow_map_fb(w: *mut i32, h: *mut i32, stride: *mut i32) -> *mut u32 {
    use oxbow_abi::{BOOT_FB, BOOT_GPU_FB, FB_MMIO, GPU_FB_H, GPU_FB_W};
    // Prefer the virtio-gpu shared fb; else fall back to the Limine fb (BOOT_FB), exactly
    // like oxcomp. `just play`/-vga std has no gpu, so the fallback is the common path.
    let (fw, fh, fpitch) = if rt::sys_shm_map(BOOT_GPU_FB, FB_MMIO).is_ok() {
        (GPU_FB_W, GPU_FB_H, GPU_FB_W * 4)
    } else {
        match rt::sys_fb_info(BOOT_FB) {
            Ok((fw, fh, fpitch, bpp)) if bpp == 32 && rt::sys_fb_map(BOOT_FB, FB_MMIO).is_ok() => {
                (fw, fh, fpitch)
            }
            _ => return core::ptr::null_mut(),
        }
    };
    unsafe {
        *w = fw as i32;
        *h = fh as i32;
        *stride = fpitch as i32;
    }
    FB_MMIO as *mut u32
}

struct MuslAlloc;
unsafe impl GlobalAlloc for MuslAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        malloc(l.size())
    }
    unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
        free(p)
    }
}

#[global_allocator]
static ALLOC: MuslAlloc = MuslAlloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { __oxbow_exit(101) }
}
