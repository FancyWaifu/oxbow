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

use oxbow_abi::{BOOT_CONSOLE, BOOT_MEM, BOOT_PCI, GPU_DMA, GPU_MMIO};
use oxbow_rt as rt;
use virtio::{r32, w32, w64, Transport, Vq};

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
/// The scanout backing store (a contiguous DMA region) lives here, past the
/// rings/cmd/resp pages at GPU_DMA+0..0x2000.
const FB_OFF: u64 = 0x10000;

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

/// Feature bits we accept: only VIRTIO_F_VERSION_1 (bit 32). (EDID/virgl later.)
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

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
            if !t.begin(VIRTIO_F_VERSION_1) {
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
}

/// Fill the scanout backing with a recognizable test pattern: a red-(x) /
/// green-(y) gradient with constant blue — confirms format + stride + flush.
fn draw_test_pattern(buf: *mut u32, w: u32, h: u32) {
    for y in 0..h {
        for x in 0..w {
            let r = x * 255 / w;
            let g = y * 255 / h;
            let b = 0x40u32;
            let px = (r << 16) | (g << 8) | b; // 0x00RRGGBB (B8G8R8X8)
            unsafe { core::ptr::write_volatile(buf.add((y * w + x) as usize), px) };
        }
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

    loop {
        core::hint::spin_loop();
    }
}
