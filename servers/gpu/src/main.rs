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
    if n > 0 && modes[0].enabled {
        w(b"[gpu] scanout0 ");
        wnum(modes[0].width);
        w(b"x");
        wnum(modes[0].height);
        w(b" (enabled)\n");
    } else {
        w(b"[gpu] scanout0 not enabled (no display attached)\n");
    }

    // Phases 2+: create a scanout resource, attach backing, draw, flush.
    loop {
        core::hint::spin_loop();
    }
}
