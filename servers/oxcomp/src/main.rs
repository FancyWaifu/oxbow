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

use core::sync::atomic::{AtomicU32, Ordering};

/// §maximize: the ACTUAL framebuffer resolution, published by `oxbow_main` once the
/// display is up so the per-app spawn budgets can be sized from it (both the boot
/// terminal and runtime Activities launches). Defaults to 1280x800 until set.
static FB_W: AtomicU32 = AtomicU32::new(1280);
static FB_H: AtomicU32 = AtomicU32::new(800);

/// §maximize: memory budget for an app that re-renders NATIVELY on maximize — its
/// working set PLUS a full-screen DOUBLE buffer at the REAL screen resolution, with
/// headroom for oxui's non-destructive realloc (it keeps the old buffer mapped until
/// the new, larger one is allocated, so the peak is ~3 full-screen buffers). Sizing
/// this from the live framebuffer makes it correct at 1280x800, 1920x1080, 4K, … — a
/// hardcoded 1280x800 assumption OOMed at 1080p, so create_shm fell back to the small
/// buffer and the compositor UPSCALED the window (the "maximize blows up the text +
/// goes slow" bug). `just play` runs at 1920x1080 (no virtio-gpu → Limine fb).
fn app_budget(working_set_mb: u64) -> u64 {
    let fb_bytes = FB_W.load(Ordering::Relaxed) as u64 * FB_H.load(Ordering::Relaxed) as u64 * 4;
    working_set_mb * 1024 * 1024 + 4 * fb_bytes
}

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

/// §92: mute/unmute the kbd→tty character path. The compositor calls this on focus
/// changes — muting (on != 0) when a non-terminal window is focused so keystrokes
/// reach only that window's wl_keyboard, not the shell. Send-only on BOOT_TTY.
#[no_mangle]
pub extern "C" fn comp_tty_mute(on: i32) {
    use oxbow_abi::{BOOT_TTY, TAG_TTY_MUTE};
    let mut m = MsgBuf::new(TAG_TTY_MUTE);
    m.data[0] = if on != 0 { 1 } else { 0 };
    m.data_len = 1;
    let _ = rt::sys_send(BOOT_TTY, &m);
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
        // §maximize: a window can be maximized to the full screen, so every app that
        // renders NATIVELY at the new size needs budget for a full-screen DOUBLE buffer
        // (2 x 1280x800x4 = ~8 MB) on top of its working set — else create_shm OOMs and
        // oxui falls back to the small buffer + the compositor upscales (blurry/slow).
        0 => (BOOT_IMG_OXTERM, app_budget(24)),
        1 => (BOOT_IMG_SYSMON, app_budget(20)),
        2 => (BOOT_IMG_WLCLIENT, app_budget(16)),
        3 => (BOOT_IMG_DOOM, 24 * 1024 * 1024), // DOOM scales (fixed 320x200), no big buffer
        _ => return -1,
    };
    let Some((srv, cli)) = rt::channel::pair() else {
        return -1;
    };
    let mut m = MsgBuf::new(0);
    m.data[0] = budget;
    m.data_len = 3;
    m.handle_count = 4;
    // §96 Phase 4: ALL oxui apps (oxterm 0, sysmon 1, wlclient 2, doom 3) are dynamically
    // linked now, so every one needs BOOT_FS_ROOT at slot 1 (ld-oxbow opens /lib/liboxui.so
    // there — they have no other fs cap; doom also opens its WAD via it), the console at
    // slot 2, and the Wayland socket at slot 4 (each app calls oxui_set_wl_slot(4)). Only
    // oxterm keeps the tty-output mirror (BOOT_TERM_CHAN) at slot 20; the rest get NULL.
    m.handles[0] = BOOT_FS_ROOT; // slot 1: filesystem (BOOT_EP / ld-oxbow's /lib + doom WAD)
    m.handles[1] = BOOT_CONSOLE; // slot 2: console (stdout)
    m.handles[2] = cli; // slot 4: Wayland socket
    m.handles[3] = if app_id == 0 { BOOT_TERM_CHAN } else { HANDLE_NULL }; // slot 20: tty mirror (oxterm)
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

    // §maximize: publish the real resolution so per-app spawn budgets fit a full-screen
    // double buffer at THIS screen size (not a hardcoded 1280x800 that OOMs at 1080p).
    FB_W.store(width, Ordering::Relaxed);
    FB_H.store(height, Ordering::Relaxed);

    // A channel pair: one end becomes the client's Wayland socket, we keep the other.
    let Some((srv_end, cli_end)) = rt::channel::pair() else {
        w(b"[oxcomp] channel pair failed\n");
        park();
    };

    // Spawn wlclient, handing it `cli_end` at spawn slot 1 (a fresh exit notif so
    // we can tell when it dies).
    let exit = rt::sys_notif_create().unwrap_or(HANDLE_NULL);
    let mut m = MsgBuf::new(0);
    m.data[0] = app_budget(24); // child Memory budget: FreeType + vterm + font + glyph
                                // cache PLUS a full-screen DOUBLE buffer for maximize, sized
                                // to the REAL resolution (§63/§maximize). 1280x800-hardcoded
                                // before → OOM at 1080p → upscaled (blurry/slow) terminal.
    m.data_len = 3; // data[1]/data[2] = empty argv
    m.handle_count = 4;
    // §96 Phase 4: oxterm is dynamically linked now (oxui in /lib/liboxui.so), so it
    // needs BOOT_FS_ROOT at slot 1 for ld-oxbow to open /lib, the Wayland socket moves
    // to slot 4 (term.c calls oxui_set_wl_slot(4)), and the tty mirror stays at slot 20.
    m.handles[0] = oxbow_abi::BOOT_FS_ROOT; // -> slot 1: filesystem (ld-oxbow's /lib)
    m.handles[1] = BOOT_CONSOLE; // -> slot 2: console (stdout / debug)
    m.handles[2] = cli_end; // -> slot 4: the Wayland socket
    m.handles[3] = BOOT_TERM_CHAN; // -> slot 20: the tty-output mirror channel (§53)
    if rt::sys_spawn(BOOT_IMG_OXTERM, BOOT_MEM, &m, exit).is_err() {
        w(b"[oxcomp] failed to spawn the terminal\n");
        park();
    }

    // §havoc: also boot havoc — the first real upstream Wayland terminal (musl) — as a
    // second window, so it renders alongside oxterm (which still handles the greeter
    // login). Fresh channel pair: havoc gets `hcli` as its Wayland socket at slot 4;
    // we keep `hsrv` and register it as a second client (srv2) below.
    let srv2 = match rt::channel::pair() {
        Some((hsrv, hcli)) => {
            let mut hm = MsgBuf::new(0);
            hm.data[0] = app_budget(24);
            hm.data_len = 3;
            hm.handle_count = 4;
            hm.handles[0] = oxbow_abi::BOOT_FS_ROOT; // slot 1: fs (musl personality + /bin/sh)
            hm.handles[1] = BOOT_CONSOLE; // slot 2: console
            hm.handles[2] = hcli; // slot 4: Wayland socket
            hm.handles[3] = HANDLE_NULL; // slot 20: havoc has no tty mirror
            if rt::sys_spawn(oxbow_abi::BOOT_IMG_HAVOC, BOOT_MEM, &hm, HANDLE_NULL).is_ok() {
                hsrv
            } else {
                w(b"[oxcomp] havoc spawn failed\n");
                HANDLE_NULL
            }
        }
        None => HANDLE_NULL,
    };
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
