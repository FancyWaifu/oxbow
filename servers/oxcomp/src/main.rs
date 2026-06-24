//! oxcomp — a tiny Wayland compositor (§42). A boot server that owns the
//! framebuffer capability and runs real libwayland: it advertises wl_compositor +
//! wl_shm, SPAWNS a separate Wayland client (`wlclient`) handing it one end of a
//! channel as its Wayland socket (the inherited-fd model), and on the client's
//! wl_surface.commit composites the client's shm buffer into the framebuffer.
//!
//! So a Wayland client's pixels reach the screen through the entire ported stack
//! (libwayland + libffi + the channel transport + shm/memfd), CROSS-PROCESS — the
//! way real Wayland works.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;

use oxbow_abi::{
    Handle, MsgBuf, BOOT_CONSOLE, BOOT_FB, BOOT_GPU_CURSOR, BOOT_GPU_FB, BOOT_IMG_OXTERM,
    BOOT_INPUT_CHAN,
    BOOT_MEM, BOOT_MOUSE_CHAN, BOOT_SESSION_CHAN, BOOT_TERM_CHAN, FB_MMIO, GPU_FB_H, GPU_FB_W,
    HANDLE_NULL,
};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Log helper for the C side (no working stdout in a boot server).
#[no_mangle]
pub extern "C" fn ox_log(p: *const u8, len: usize) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, p, len);
}

/// Milliseconds since boot — the timestamp the compositor stamps on frame-callback
/// `done` events; Wayland clients animate off its delta.
#[no_mangle]
pub extern "C" fn ox_now_ms() -> u32 {
    rt::sys_uptime_ms() as u32
}

/// §91: launch an app at runtime when the user clicks it in the Activities
/// overview. Spawns the image, handing it a fresh Wayland-socket channel; returns
/// the compositor's end as an fd for `wl_client_create`, or -1 on failure.
/// app id: 0 = Terminal (oxterm), 1 = Monitor (sysmon), 2 = Rings (wlclient).
#[no_mangle]
pub extern "C" fn comp_server_launch_app(app_id: i32) -> i32 {
    use oxbow_abi::{
        BOOT_FS_ROOT, BOOT_IMG_DOOM, BOOT_IMG_OXTERM, BOOT_IMG_SYSMON, BOOT_IMG_WLCLIENT,
        BOOT_TERM_CHAN,
    };
    let (img, budget): (Handle, u64) = match app_id {
        0 => (BOOT_IMG_OXTERM, 36 * 1024 * 1024),
        1 => (BOOT_IMG_SYSMON, 24 * 1024 * 1024),
        2 => (BOOT_IMG_WLCLIENT, 16 * 1024 * 1024),
        3 => (BOOT_IMG_DOOM, 24 * 1024 * 1024), // TEMP: test sysmon's working budget
        _ => return -1,
    };
    let Some((srv, cli)) = rt::channel::pair() else {
        return -1;
    };
    let mut m = MsgBuf::new(0);
    m.data[0] = budget;
    m.data_len = 3;
    m.handle_count = 4;
    if app_id == 3 {
        // DOOM: filesystem cap on slot 1 (BOOT_EP, so doomgeneric opens /doom1.wad via
        // stdio), console on slot 2 (BOOT_CONSOLE — oxbow-libc's stdout, routed to the
        // console by rt::stdout_write's mode-3 fallback), and the Wayland socket on slot
        // 4 (oxui's `oxui_wl_slot` is set to 4 to match).
        m.handles[0] = BOOT_FS_ROOT; // slot 1: filesystem (BOOT_EP)
        m.handles[1] = BOOT_CONSOLE; // slot 2: console (stdout)
        m.handles[2] = cli; // slot 4: Wayland socket
        m.handles[3] = HANDLE_NULL; // slot 20: unused
    } else {
        m.handles[0] = cli; // slot 1: Wayland socket
        m.handles[1] = HANDLE_NULL; // slot 2: stdout (unused)
        m.handles[2] = BOOT_CONSOLE; // slot 4: debug console
        m.handles[3] = if app_id == 0 { BOOT_TERM_CHAN } else { HANDLE_NULL }; // slot 20: tty mirror
    }
    if rt::sys_spawn(img, BOOT_MEM, &m, HANDLE_NULL).is_ok() {
        w(b"[oxcomp] launched app from Activities\n");
        unsafe { ox_chan_fd(srv as u32) }
    } else {
        w(b"[oxcomp] launch failed (out of budget?)\n");
        let _ = rt::sys_close(srv);
        let _ = rt::sys_close(cli);
        -1
    }
}

extern "C" {
    fn comp_server_setup(
        fd: i32,
        input_fd: i32,
        mouse_fd: i32,
        session_fd: i32,
        fb: *mut u32,
        w: i32,
        h: i32,
        pitch_words: i32,
    ) -> *mut u8;
    fn comp_server_pump(d: *mut u8);
    fn comp_server_add_client(d: *mut u8, fd: i32);
    fn comp_server_composited() -> i32;
    /// §90: publish the cursor position into a shared region for the GPU hardware
    /// cursor instead of painting it (NULL = software cursor).
    fn comp_server_set_hwcursor(region: *mut u32);
    /// Wrap a channel capability handle as a stream fd (libc, ox_chan_fd).
    fn ox_chan_fd(handle: u32) -> i32;
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // Compositor-over-GPU (§90): prefer the kernel-shared GPU framebuffer — map it
    // at FB_MMIO and composite straight into it; the gpu driver scans it out. So
    // the virtio-gpu is the real display, no Limine framebuffer needed. Fall back
    // to the Limine fb (BOOT_FB) when no GPU is present.
    let (width, height, pitch): (u32, u32, u32) =
        if rt::sys_shm_map(BOOT_GPU_FB, FB_MMIO).is_ok() {
            w(b"[oxcomp] compositing into the GPU shared framebuffer\n");
            (GPU_FB_W, GPU_FB_H, GPU_FB_W * 4)
        } else {
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
            (width, height, pitch)
        };

    // A channel pair: one end becomes the client's Wayland socket, we keep the other.
    let Some((srv_end, cli_end)) = rt::channel::pair() else {
        w(b"[oxcomp] channel pair failed\n");
        park();
    };

    // Spawn wlclient, handing it `cli_end` at spawn slot 1 (a fresh exit notif so
    // we can tell when it dies).
    let exit = rt::sys_notif_create().unwrap_or(HANDLE_NULL);
    let mut m = MsgBuf::new(0);
    m.data[0] = 36 * 1024 * 1024; // child Memory budget (FreeType + vterm + font +
                                  // TWO 1.15 MB shm buffers for double-buffering; §63)
    m.data_len = 3; // data[1]/data[2] = empty argv
    m.handle_count = 4;
    m.handles[0] = cli_end; // -> child slot 1 (the Wayland socket)
    m.handles[1] = HANDLE_NULL; // slot 2 (stdout) — unused
    m.handles[2] = BOOT_CONSOLE; // -> child slot 4: a console for debug logging
    m.handles[3] = BOOT_TERM_CHAN; // -> child slot 20: the tty-output mirror channel (§53)
    if rt::sys_spawn(BOOT_IMG_OXTERM, BOOT_MEM, &m, exit).is_err() {
        w(b"[oxcomp] failed to spawn the terminal\n");
        park();
    }

    // §91: a GNOME-clean start — only the terminal opens at boot; the rest of the
    // apps (Monitor, Rings, more terminals) are launched on demand from the
    // Activities overview. That keeps the desktop uncluttered and leaves the
    // compositor's Memory budget free to fund runtime launches.
    let srv2 = HANDLE_NULL;
    let srv3 = HANDLE_NULL;
    w(b"[oxcomp] compositor up; terminal spawned (launch more from Activities)\n");

    // Set up the display on our kept channel end and run the compositing loop.
    // The keyboard channel (from the kbd driver, §47) becomes a second fd the
    // event loop watches for input.
    let server_fd = unsafe { ox_chan_fd(srv_end as u32) };
    let input_fd = unsafe { ox_chan_fd(BOOT_INPUT_CHAN as u32) };
    let mouse_fd = unsafe { ox_chan_fd(BOOT_MOUSE_CHAN as u32) };
    // §92: the session channel to the shell — the greeter relays credentials over
    // it as a byte stream and watches it for the logout signal.
    let session_fd = unsafe { ox_chan_fd(BOOT_SESSION_CHAN as u32) };
    let display = unsafe {
        comp_server_setup(
            server_fd,
            input_fd,
            mouse_fd,
            session_fd,
            FB_MMIO as *mut u32,
            width as i32,
            height as i32,
            (pitch / 4) as i32,
        )
    };
    if display.is_null() {
        w(b"[oxcomp] display setup failed\n");
        park();
    }
    // §90 Phase 4: if the gpu shared a cursor-state region, map it and switch to
    // the hardware cursor — we publish the pointer position there and the gpu's
    // device cursor composites it (no software arrow painted into the fb).
    const CURSOR_VADDR: u64 = 0x5100_0000;
    if rt::sys_shm_map(BOOT_GPU_CURSOR, CURSOR_VADDR).is_ok() {
        unsafe { comp_server_set_hwcursor(CURSOR_VADDR as *mut u32) };
        w(b"[oxcomp] hardware cursor (GPU) enabled\n");
    }
    // §56: attach the second client to the display.
    if srv2 != HANDLE_NULL {
        let fd2 = unsafe { ox_chan_fd(srv2 as u32) };
        unsafe { comp_server_add_client(display, fd2) };
    }
    // §64: attach the third client (sysmon).
    if srv3 != HANDLE_NULL {
        let fd3 = unsafe { ox_chan_fd(srv3 as u32) };
        unsafe { comp_server_add_client(display, fd3) };
    }
    // Pump the compositor. We busy-poll epoll, so the cross-process client only
    // makes progress in its own time slices — give it real wall-clock time (a
    // deadline, not an iteration count), and keep compositing a while after the
    // first frame so animation settles before a screen capture.
    let start = rt::sys_uptime_ms();
    let mut announced = false;
    loop {
        unsafe { comp_server_pump(display) };
        if unsafe { comp_server_composited() } != 0 {
            if !announced {
                // Once a client's frame lands, keep compositing forever: each
                // commit fires a frame callback, the client redraws, and the
                // surface animates. The compositor is a service — it never parks.
                w(b"[oxcomp] composited a client surface (animating)\n");
                announced = true;
            }
        } else if rt::sys_uptime_ms() - start > 15000 {
            w(b"[oxcomp] no surface composited\n");
            park(); // no client showed up — give up and idle
        }
    }
}

fn park() -> ! {
    if let Ok(ep) = rt::sys_ep_create() {
        let mut m = MsgBuf::new(0);
        loop {
            let _ = rt::sys_recv(ep, &mut m);
        }
    }
    loop {}
}

// Keep these referenced so the handle type is used.
#[allow(dead_code)]
fn _t(_: Handle) {}
