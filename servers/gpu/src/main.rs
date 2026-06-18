//! gpu — a userspace virtio-gpu (modern/PCI) display driver.
//!
//! Owns the virtio-gpu device the kernel hands it (PciDevice cap at BOOT_PCI, DMA
//! from its Memory budget, IRQ cap for async events). It brings the device up over
//! the modern-virtio transport (see `virtio.rs`), then drives the 2D command set:
//! create a host scanout resource, attach guest-memory backing, set it on a
//! scanout, and TRANSFER + FLUSH dirty regions. This is oxbow's own GPU command
//! submission — not Limine's static framebuffer.
//!
//! Phase 1 (this file's first cut): bring-up + GET_DISPLAY_INFO. Scanout, the
//! compositor display protocol, and the hardware cursor land in later phases.
#![no_std]
#![no_main]

mod virtio;

use oxbow_abi::{
    BOOT_CONSOLE, BOOT_GPU_CURSOR, BOOT_GPU_FB, BOOT_GPU_IRQ, BOOT_MEM, BOOT_PCI, GPU_DMA, GPU_FB_H,
    GPU_FB_W, GPU_MMIO,
};
use oxbow_rt as rt;
use virtio::{r32, w32, w64, Transport, Vq};

// Cursor-queue command types (§90 Phase 4) + the cursor resource/format.
const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;
const CURSOR_RES_ID: u32 = 2;
const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1; // cursor needs alpha (transparent bg)
const CURSORQ_OFF: u64 = 0x3000; // cursor virtqueue rings
const CURSOR_BK_OFF: u64 = 0x5000; // 64x64x4 cursor image backing (4 pages)
const CURSOR_STATE_OFF: u64 = 0x20000; // where the gpu maps the shared cursor-state region

// 2D command types.
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
/// B8G8R8X8: memory byte order B,G,R,X — a u32 pixel is 0x00RRGGBB little-endian,
/// matching the Limine framebuffer convention.
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
/// virtio-gpu async event: the host display configuration changed (resize/hotplug).
const VIRTIO_GPU_EVENT_DISPLAY: u32 = 1 << 0;
/// virtio_pci_isr_status bit: the device configuration changed (not a queue event).
const VIRTIO_PCI_ISR_CONFIG: u8 = 1 << 1;

/// The scanout backing store (a contiguous DMA region) lives here, past the
/// rings/cmd/resp pages at GPU_DMA+0..0x2000. A second backing (for a runtime
/// modeset to a new resolution) lives at FB_OFF2, past the first (max ~8 MiB).
const FB_OFF: u64 = 0x10000;
const FB_OFF2: u64 = 0x810000;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Log a u32 as decimal (boot drivers have no stdout; they write the console cap).
fn wnum(mut n: u32) {
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
    w(&b[i..]);
}

// --- virtio-gpu protocol ------------------------------------------------------
// virtio_gpu_ctrl_hdr: type u32, flags u32, fence_id u64, ctx_id u32, padding u32.
const HDR_LEN: usize = 24;
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
// virtio_gpu_config (device cfg): events_read, events_clear, num_scanouts, num_capsets.
const CFG_NUM_SCANOUTS: usize = 8;

/// Feature bits we accept: VIRTIO_F_VERSION_1 (bit 32) + VIRTIO_GPU_F_EDID (bit 1,
/// for GET_EDID — negotiated if the device offers it). (virgl/3D later.)
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_GPU_F_EDID: u64 = 1 << 1;
const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;
const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;

/// DMA layout within the gpu's reserved GPU_DMA range (one 4 KiB page each):
const CTRLQ_OFF: u64 = 0x0000; // control virtqueue rings
const CMD_OFF: u64 = 0x1000; // command buffer (device reads)
const RESP_OFF: u64 = 0x2000; // response buffer (device writes)

/// A displayable mode reported by the device for one scanout.
#[derive(Clone, Copy, Default)]
struct Scanout {
    width: u32,
    height: u32,
    enabled: bool,
}

/// The brought-up virtio-gpu device + its control queue and command buffers.
struct Gpu {
    t: Transport,
    ctrlq: Vq,
    cursorq: Option<Vq>,
    cmd_v: usize,
    cmd_p: u64,
    resp_v: usize,
    resp_p: u64,
}

impl Gpu {
    /// Probe, handshake, set up the control queue, and go live. None on any failure
    /// (no device / feature rejection / DMA exhaustion).
    fn bring_up() -> Option<Gpu> {
        let t = Transport::probe(BOOT_PCI, GPU_MMIO)?;
        unsafe {
            if !t.begin(VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_EDID) {
                w(b"[gpu] device rejected features\n");
                return None;
            }
        }
        // Control queue rings + command/response buffers from our DMA budget.
        let ring_p = rt::sys_dma_alloc(BOOT_MEM, GPU_DMA + CTRLQ_OFF).unwrap_or(0);
        let cmd_p = rt::sys_dma_alloc(BOOT_MEM, GPU_DMA + CMD_OFF).unwrap_or(0);
        let resp_p = rt::sys_dma_alloc(BOOT_MEM, GPU_DMA + RESP_OFF).unwrap_or(0);
        if ring_p == 0 || cmd_p == 0 || resp_p == 0 {
            w(b"[gpu] DMA alloc failed\n");
            return None;
        }
        let ctrlq = unsafe { t.setup_queue(0, (GPU_DMA + CTRLQ_OFF) as usize, ring_p) };
        unsafe { t.finish() }; // DRIVER_OK
        Some(Gpu {
            t,
            ctrlq,
            cursorq: None,
            cmd_v: (GPU_DMA + CMD_OFF) as usize,
            cmd_p,
            resp_v: (GPU_DMA + RESP_OFF) as usize,
            resp_p,
        })
    }

    /// Number of scanouts (displays) the device reports.
    fn num_scanouts(&self) -> u32 {
        unsafe { r32(self.t.device + CFG_NUM_SCANOUTS) }
    }

    /// Write a control header (type + zeros) into the command buffer.
    unsafe fn put_hdr(&self, cmd_type: u32) {
        let c = self.cmd_v;
        w32(c, cmd_type);
        w32(c + 4, 0); // flags
        w64(c + 8, 0); // fence_id
        w32(c + 16, 0); // ctx_id
        w32(c + 20, 0); // padding
    }

    /// Submit a command of `req_len` bytes (already written to cmd buffer) and wait
    /// for a `resp_len`-byte response. Returns the response type, or 0 on timeout.
    unsafe fn submit(&mut self, req_len: u32, resp_len: u32) -> u32 {
        if !self.ctrlq.request(self.cmd_p, req_len, self.resp_p, resp_len) {
            return 0;
        }
        r32(self.resp_v) // response hdr.type
    }

    /// GET_DISPLAY_INFO → fill `out` with up to VIRTIO_GPU_MAX_SCANOUTS modes,
    /// returning the count parsed. Each virtio_gpu_display_one is 24 bytes:
    /// rect{x,y,width,height} (4xu32) + enabled u32 + flags u32, after the 24-byte
    /// response header.
    fn display_info(&mut self, out: &mut [Scanout; VIRTIO_GPU_MAX_SCANOUTS]) -> usize {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO);
            let resp_len = (HDR_LEN + VIRTIO_GPU_MAX_SCANOUTS * 24) as u32;
            if self.submit(HDR_LEN as u32, resp_len) != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
                return 0;
            }
            for i in 0..VIRTIO_GPU_MAX_SCANOUTS {
                let p = self.resp_v + HDR_LEN + i * 24;
                out[i] = Scanout {
                    width: r32(p + 8),
                    height: r32(p + 12),
                    enabled: r32(p + 16) != 0,
                };
            }
            VIRTIO_GPU_MAX_SCANOUTS
        }
    }

    /// GET_EDID for scanout 0 (§90, Phase 5). Returns (edid_size, valid) where
    /// `valid` checks the standard EDID magic header. None if the device lacks the
    /// EDID feature. The blob (display capabilities/timings) follows hdr+size+pad.
    fn get_edid(&mut self) -> Option<(u32, bool)> {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_GET_EDID);
            let c = self.cmd_v;
            w32(c + 24, 0); // scanout id
            w32(c + 28, 0); // padding
            let resp_len = (HDR_LEN + 8 + 1024) as u32; // hdr + size + padding + edid[1024]
            if self.submit(32, resp_len) != VIRTIO_GPU_RESP_OK_EDID {
                return None;
            }
            let size = r32(self.resp_v + HDR_LEN);
            // EDID magic: 00 FF FF FF FF FF FF 00 at the blob start.
            let blob = self.resp_v + HDR_LEN + 8;
            let valid = virtio::r8(blob) == 0x00
                && virtio::r8(blob + 1) == 0xFF
                && virtio::r8(blob + 7) == 0x00;
            Some((size, valid))
        }
    }

    /// Create a host 2D resource (`id`, `fmt`, `w`x`h`).
    fn create_2d(&mut self, id: u32, fmt: u32, w: u32, h: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
            let c = self.cmd_v;
            w32(c + 24, id);
            w32(c + 28, fmt);
            w32(c + 32, w);
            w32(c + 36, h);
            self.submit(40, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Attach a single contiguous backing region (`phys`,`len`) to resource `id`.
    fn attach_backing(&mut self, id: u32, phys: u64, len: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
            let c = self.cmd_v;
            w32(c + 24, id);
            w32(c + 28, 1); // nr_entries = 1 (contiguous, no scatter-gather)
            w64(c + 32, phys); // virtio_gpu_mem_entry.addr
            w32(c + 40, len); // .length
            w32(c + 44, 0); // .padding
            self.submit(48, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Bind resource `id` to `scanout` covering its full `w`x`h`.
    fn set_scanout(&mut self, scanout: u32, id: u32, w: u32, h: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_SET_SCANOUT);
            let c = self.cmd_v;
            w32(c + 24, 0); // rect.x
            w32(c + 28, 0); // rect.y
            w32(c + 32, w); // rect.width
            w32(c + 36, h); // rect.height
            w32(c + 40, scanout);
            w32(c + 44, id);
            self.submit(48, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Handle a device config-change interrupt (§90): read the ISR (clears it),
    /// and if a DISPLAY event is pending, clear it, re-query the display geometry,
    /// and re-bind the scanout (the shared fb stays its fixed size; the device
    /// scales). Returns true if a display change was handled.
    fn handle_config_change(&mut self) -> bool {
        unsafe {
            let isr = virtio::r8(self.t.isr); // read-to-clear
            if isr & VIRTIO_PCI_ISR_CONFIG == 0 {
                return false;
            }
            let events = r32(self.t.device); // virtio_gpu_config.events_read
            if events & VIRTIO_GPU_EVENT_DISPLAY == 0 {
                return false;
            }
            w32(self.t.device + 4, events); // events_clear — ack the event
        }
        let mut modes = [Scanout::default(); VIRTIO_GPU_MAX_SCANOUTS];
        self.display_info(&mut modes);
        w(b"[gpu] config-change: scanout0 ");
        wnum(modes[0].width);
        w(b"x");
        wnum(modes[0].height);
        w(b"\n");
        // Re-bind the scanout (host may have invalidated it). The compositor still
        // renders GPU_FB_W x GPU_FB_H; the device scales to the new display.
        self.set_scanout(0, 1, GPU_FB_W, GPU_FB_H);
        true
    }

    /// Copy guest backing -> host resource for the full `w`x`h` rect.
    fn transfer(&mut self, id: u32, w: u32, h: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
            let c = self.cmd_v;
            w32(c + 24, 0); // rect.x
            w32(c + 28, 0); // rect.y
            w32(c + 32, w); // rect.width
            w32(c + 36, h); // rect.height
            w64(c + 40, 0); // offset into backing
            w32(c + 48, id);
            w32(c + 52, 0); // padding
            self.submit(56, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Copy a SUB-rectangle of the backing to the host resource. `offset` is the
    /// byte offset of the rect's origin in the backing (= (y*stride_w + x)*4), so
    /// the device reads the right rows. Dirty-rect transfer — far cheaper than the
    /// full frame.
    fn transfer_rect(&mut self, id: u32, x: u32, y: u32, rw: u32, rh: u32, stride_w: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
            let c = self.cmd_v;
            w32(c + 24, x);
            w32(c + 28, y);
            w32(c + 32, rw);
            w32(c + 36, rh);
            w64(c + 40, ((y * stride_w + x) * 4) as u64); // offset into backing
            w32(c + 48, id);
            w32(c + 52, 0);
            self.submit(56, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Flush a SUB-rectangle of the host resource to the display.
    fn flush_rect(&mut self, id: u32, x: u32, y: u32, rw: u32, rh: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
            let c = self.cmd_v;
            w32(c + 24, x);
            w32(c + 28, y);
            w32(c + 32, rw);
            w32(c + 36, rh);
            w32(c + 40, id);
            w32(c + 44, 0);
            self.submit(48, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Flush the host resource to the display for the full `w`x`h` rect.
    fn flush(&mut self, id: u32, w: u32, h: u32) -> bool {
        unsafe {
            self.put_hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
            let c = self.cmd_v;
            w32(c + 24, 0); // rect.x
            w32(c + 28, 0); // rect.y
            w32(c + 32, w); // rect.width
            w32(c + 36, h); // rect.height
            w32(c + 40, id);
            w32(c + 44, 0); // padding
            self.submit(48, HDR_LEN as u32) == VIRTIO_GPU_RESP_OK_NODATA
        }
    }

    /// Stand up the HARDWARE cursor (§90 Phase 4): bring up the cursor virtqueue
    /// (queue 1), create a 64x64 BGRA cursor resource with an arrow drawn into it,
    /// and UPDATE_CURSOR to bind it. Returns false on any failure.
    fn setup_cursor(&mut self) -> bool {
        let ring_p = rt::sys_dma_alloc(BOOT_MEM, GPU_DMA + CURSORQ_OFF).unwrap_or(0);
        let bk_p = rt::sys_dma_alloc_contig(BOOT_MEM, GPU_DMA + CURSOR_BK_OFF, 4).unwrap_or(0);
        if ring_p == 0 || bk_p == 0 {
            return false;
        }
        self.cursorq = Some(unsafe { self.t.setup_queue(1, (GPU_DMA + CURSORQ_OFF) as usize, ring_p) });
        if !(self.create_2d(CURSOR_RES_ID, VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM, 64, 64)
            && self.attach_backing(CURSOR_RES_ID, bk_p, 64 * 64 * 4))
        {
            return false;
        }
        draw_cursor_arrow((GPU_DMA + CURSOR_BK_OFF) as *mut u32);
        self.transfer(CURSOR_RES_ID, 64, 64);
        unsafe { self.cursor_cmd(VIRTIO_GPU_CMD_UPDATE_CURSOR, 0, 0, CURSOR_RES_ID) }
    }

    /// Submit an UPDATE/MOVE cursor command on the cursor queue. UPDATE binds the
    /// resource + hotspot; MOVE just repositions. Returns false if no cursor queue.
    unsafe fn cursor_cmd(&mut self, cmd_type: u32, x: u32, y: u32, res: u32) -> bool {
        let c = self.cmd_v;
        w32(c, cmd_type);
        w32(c + 4, 0);
        w64(c + 8, 0);
        w32(c + 16, 0);
        w32(c + 20, 0);
        // virtio_gpu_cursor_pos: scanout_id, x, y, padding
        w32(c + 24, 0);
        w32(c + 28, x);
        w32(c + 32, y);
        w32(c + 36, 0);
        // resource_id, hot_x, hot_y, padding
        w32(c + 40, res);
        w32(c + 44, 0);
        w32(c + 48, 0);
        w32(c + 52, 0);
        let (cmd_p, resp_p) = (self.cmd_p, self.resp_p);
        match self.cursorq.as_mut() {
            Some(cq) => cq.request(cmd_p, 56, resp_p, HDR_LEN as u32),
            None => false,
        }
    }
}

/// Draw a classic arrow pointer into a 64x64 BGRA cursor backing (rest stays
/// transparent — the backing is zeroed). 'X' = black outline, '.' = white fill.
fn draw_cursor_arrow(buf: *mut u32) {
    const ARROW: [&[u8]; 17] = [
        b"X          ", b"XX         ", b"X.X        ", b"X..X       ",
        b"X...X      ", b"X....X     ", b"X.....X    ", b"X......X   ",
        b"X.......X  ", b"X........X ", b"X.....XXXXX", b"X..X..X    ",
        b"X.X X..X   ", b"XX  X..X   ", b"X    X..X  ", b"     X..X  ",
        b"      XX   ",
    ];
    for (j, row) in ARROW.iter().enumerate() {
        for (i, &c) in row.iter().enumerate() {
            let px = match c {
                b'X' => 0xFF00_0000u32, // black, opaque (A,R,G,B = FF,00,00,00)
                b'.' => 0xFFFF_FFFFu32, // white, opaque
                _ => continue,          // transparent
            };
            unsafe { core::ptr::write_volatile(buf.add(j * 64 + i), px) };
        }
    }
}

/// The gradient color at (x,y): red along x, green along y, constant blue.
#[inline]
fn grad(x: u32, y: u32, w: u32, h: u32) -> u32 {
    ((x * 255 / w) << 16) | ((y * 255 / h) << 8) | 0x40 // 0x00RRGGBB (B8G8R8X8)
}

/// Fill the whole backing with the gradient test pattern.
fn draw_test_pattern(buf: *mut u32, w: u32, h: u32) {
    for y in 0..h {
        for x in 0..w {
            unsafe { core::ptr::write_volatile(buf.add((y * w + x) as usize), grad(x, y, w, h)) };
        }
    }
}

/// Repaint the gradient over a rectangle (restores the background under a sprite).
fn fill_grad_rect(buf: *mut u32, w: u32, h: u32, x0: u32, y0: u32, rw: u32, rh: u32) {
    for y in y0..(y0 + rh).min(h) {
        for x in x0..(x0 + rw).min(w) {
            unsafe { core::ptr::write_volatile(buf.add((y * w + x) as usize), grad(x, y, w, h)) };
        }
    }
}

/// Fill a rectangle with a solid color.
fn fill_rect(buf: *mut u32, w: u32, h: u32, x0: u32, y0: u32, rw: u32, rh: u32, color: u32) {
    for y in y0..(y0 + rh).min(h) {
        for x in x0..(x0 + rw).min(w) {
            unsafe { core::ptr::write_volatile(buf.add((y * w + x) as usize), color) };
        }
    }
}

/// Crude busy-wait between animation frames (no sleep syscall yet).
fn frame_delay() {
    for _ in 0..2_000_000u64 {
        core::hint::spin_loop();
    }
}

/// The display loop (Phase 3): bounce a white sprite across the gradient, doing a
/// TRANSFER + FLUSH every frame — sustained dynamic submission, the thing a
/// compositor needs. Dirty-rect: only repaint the sprite's old/new cells on the
/// CPU; the full-frame transfer is a cheap host-side DMA copy. Never returns.
fn animate(gpu: &mut Gpu, id: u32, buf: *mut u32, width: u32, height: u32) -> ! {
    let sz = 80u32;
    let y0 = height / 2 - sz / 2;
    let (mut x, mut dx, mut prev) = (0u32, 8i32, 0u32);
    let mut frame = 0u32;
    loop {
        fill_grad_rect(buf, width, height, prev, y0, sz, sz); // erase previous sprite
        fill_rect(buf, width, height, x, y0, sz, sz, 0x00FF_FFFF); // draw white sprite
        // Dirty rect = bounding box of the old + new sprite cells; transfer/flush
        // only that — a compositor's damage region, not the whole frame.
        let dx0 = prev.min(x);
        let dw = (prev.max(x) + sz) - dx0;
        gpu.transfer_rect(id, dx0, y0, dw, sz, width);
        gpu.flush_rect(id, dx0, y0, dw, sz);
        if frame % 64 == 0 {
            w(b"[gpu] frame ");
            wnum(frame);
            w(b" x=");
            wnum(x);
            w(b"\n");
        }
        frame += 1;
        prev = x;
        let nx = x as i32 + dx;
        if nx < 0 || nx as u32 + sz >= width {
            dx = -dx; // bounce
        } else {
            x = nx as u32;
        }
        frame_delay();
    }
}

/// Present the kernel-shared framebuffer (§90): bind it as the scanout backing,
/// then loop TRANSFER + FLUSH so whatever oxcomp composites into it reaches the
/// display. The gpu owns no pixels here — the compositor does. Never returns.
fn present_shared_fb(gpu: &mut Gpu, fb_phys: u64) -> ! {
    const RES: u32 = 1;
    let bytes = GPU_FB_W * GPU_FB_H * 4;
    if !(gpu.create_2d(RES, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM, GPU_FB_W, GPU_FB_H)
        && gpu.attach_backing(RES, fb_phys, bytes)
        && gpu.set_scanout(0, RES, GPU_FB_W, GPU_FB_H))
    {
        w(b"[gpu] shared-fb scanout setup failed\n");
        loop {
            core::hint::spin_loop();
        }
    }
    // Hardware cursor: map the shared cursor-state region oxcomp writes into and
    // stand up the device cursor, then track it in the present loop.
    let cursor_state = if rt::sys_shm_map(BOOT_GPU_CURSOR, GPU_DMA + CURSOR_STATE_OFF).is_ok()
        && gpu.setup_cursor()
    {
        w(b"[gpu] hardware cursor enabled\n");
        Some((GPU_DMA + CURSOR_STATE_OFF) as *const u32)
    } else {
        w(b"[gpu] hardware cursor unavailable\n");
        None
    };

    // Config-change IRQ path: suppress queue-completion interrupts (we poll the
    // used ring), then bind the device interrupt to a notification so an async
    // config-change (display resize/hotplug) wakes us — checked non-blocking each
    // present so the loop never stalls.
    unsafe {
        gpu.ctrlq.suppress_interrupts();
        if let Some(cq) = gpu.cursorq.as_ref() {
            cq.suppress_interrupts();
        }
    }
    let irq_notif = rt::sys_notif_create().unwrap_or(oxbow_abi::HANDLE_NULL);
    let irq_armed = irq_notif != oxbow_abi::HANDLE_NULL
        && rt::sys_irq_bind(BOOT_GPU_IRQ, irq_notif).is_ok()
        && rt::sys_irq_ack(BOOT_GPU_IRQ).is_ok();
    if irq_armed {
        w(b"[gpu] config-change IRQ bound\n");
    }

    w(b"[gpu] presenting oxcomp via shared framebuffer\n");
    let (mut last_x, mut last_y) = (u32::MAX, u32::MAX);
    loop {
        gpu.transfer(RES, GPU_FB_W, GPU_FB_H);
        gpu.flush(RES, GPU_FB_W, GPU_FB_H);
        // Async config-change (display resize/hotplug): non-blocking check.
        if irq_armed && rt::sys_notif_poll(irq_notif) > 0 {
            gpu.handle_config_change();
            let _ = rt::sys_irq_ack(BOOT_GPU_IRQ); // re-arm the line
        }
        // Track the pointer: oxcomp writes its position into the shared region; we
        // reposition the device's hardware cursor when it changes.
        if let Some(cs) = cursor_state {
            let (x, y) = unsafe { (core::ptr::read_volatile(cs), core::ptr::read_volatile(cs.add(1))) };
            if x != last_x || y != last_y {
                unsafe { gpu.cursor_cmd(VIRTIO_GPU_CMD_MOVE_CURSOR, x, y, 0) };
                last_x = x;
                last_y = y;
            }
        }
        frame_delay();
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"[gpu] virtio-gpu init\n");
    let mut gpu = match Gpu::bring_up() {
        Some(g) => g,
        None => {
            w(b"[gpu] no virtio-gpu device - idle\n");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    w(b"[gpu] up - scanouts=");
    wnum(gpu.num_scanouts());
    w(b"\n");

    let mut modes = [Scanout::default(); VIRTIO_GPU_MAX_SCANOUTS];
    let n = gpu.display_info(&mut modes);

    // Phase 5: query the panel's EDID (display capabilities), if the device offers it.
    match gpu.get_edid() {
        Some((size, valid)) => {
            w(b"[gpu] EDID ");
            wnum(size);
            w(if valid { b" bytes (valid header)\n" } else { b" bytes (no magic)\n" });
        }
        None => w(b"[gpu] EDID not supported by device\n"),
    }

    // Compositor-over-GPU: if the kernel pre-shared a framebuffer (the one oxcomp
    // composites into), set it as the scanout backing and present it forever. The
    // gpu never draws — it just pushes oxcomp's pixels to the display.
    if let Ok(fb_phys) = rt::sys_shm_phys(BOOT_GPU_FB) {
        if fb_phys != 0 {
            present_shared_fb(&mut gpu, fb_phys);
        }
    }
    w(b"[gpu] no shared framebuffer - standalone demo\n");

    let (mut width, mut height) = (modes[0].width, modes[0].height);
    if n == 0 || width == 0 || height == 0 {
        // No display info — fall back to a common default so we still scan out.
        width = 1280;
        height = 800;
        w(b"[gpu] no display info; defaulting to 1280x800\n");
    } else {
        w(b"[gpu] scanout0 ");
        wnum(width);
        w(b"x");
        wnum(height);
        w(b"\n");
    }

    // --- Phase 2: stand up a scanout resource backed by contiguous DMA, draw a
    // test pattern into it, and flush it to the display. ---
    const RES_ID: u32 = 1;
    let bytes = width * height * 4;
    let pages = ((bytes as u64) + 4095) / 4096;
    let fb_vaddr = GPU_DMA + FB_OFF;
    let fb_phys = rt::sys_dma_alloc_contig(BOOT_MEM, fb_vaddr, pages).unwrap_or(0);
    if fb_phys == 0 {
        w(b"[gpu] scanout backing alloc failed\n");
        loop {
            core::hint::spin_loop();
        }
    }

    let ok = gpu.create_2d(RES_ID, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM, width, height)
        && gpu.attach_backing(RES_ID, fb_phys, bytes)
        && gpu.set_scanout(0, RES_ID, width, height);
    if !ok {
        w(b"[gpu] scanout setup failed\n");
        loop {
            core::hint::spin_loop();
        }
    }

    draw_test_pattern(fb_vaddr as *mut u32, width, height);
    if gpu.transfer(RES_ID, width, height) && gpu.flush(RES_ID, width, height) {
        w(b"[gpu] scanout live - test pattern flushed\n");
    } else {
        w(b"[gpu] transfer/flush failed\n");
    }

    // --- Phase 3: runtime modeset + a continuous display loop. Switch to a NEW
    // resolution at runtime (a fresh resource on scanout 0) — something a fixed
    // Limine framebuffer can't do — then drive a sustained TRANSFER+FLUSH loop. ---
    const MODE_W: u32 = 1024;
    const MODE_H: u32 = 768;
    const RES2_ID: u32 = 2;
    let mbytes = MODE_W * MODE_H * 4;
    let mpages = ((mbytes as u64) + 4095) / 4096;
    let mfb_v = GPU_DMA + FB_OFF2;
    let mfb_p = rt::sys_dma_alloc_contig(BOOT_MEM, mfb_v, mpages).unwrap_or(0);
    if mfb_p != 0
        && gpu.create_2d(RES2_ID, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM, MODE_W, MODE_H)
        && gpu.attach_backing(RES2_ID, mfb_p, mbytes)
        && gpu.set_scanout(0, RES2_ID, MODE_W, MODE_H)
    {
        w(b"[gpu] modeset to 1024x768 - animating\n");
        draw_test_pattern(mfb_v as *mut u32, MODE_W, MODE_H);
        // Push the full gradient to the host resource once; animate() then only
        // transfers the sprite's dirty rect each frame.
        gpu.transfer(RES2_ID, MODE_W, MODE_H);
        gpu.flush(RES2_ID, MODE_W, MODE_H);
        animate(&mut gpu, RES2_ID, mfb_v as *mut u32, MODE_W, MODE_H);
    }
    w(b"[gpu] modeset failed - holding static scanout\n");
    loop {
        core::hint::spin_loop();
    }
}
