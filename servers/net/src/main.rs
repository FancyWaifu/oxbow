//! net — the e1000 network driver (resident boot module).
//!
//! This arc turns the bare PCI/MMIO capability (§18) into a working NIC driver:
//! DMA-allocated TX/RX descriptor rings, the e1000 reset + ring + RCTL/TCTL
//! bring-up, and an interrupt-driven receive path. It proves the whole stack
//! end to end by hand-building one broadcast ARP request for the QEMU SLIRP
//! gateway (10.0.2.2), transmitting it, and receiving the gateway's ARP reply
//! via the NIC's interrupt — so TX, RX, and IRQ all work over the capability
//! model. Ethernet/ARP/IP/UDP as proper layers come in the following arcs.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use oxbow_abi::{BOOT_CONSOLE, BOOT_MEM, BOOT_NET_IRQ, BOOT_PCI, NET_DMA, NET_MMIO};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

// --- e1000 register offsets (bytes into BAR0) ------------------------------
const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const ICR: usize = 0x00C0;
const IMS: usize = 0x00D0;
const IMC: usize = 0x00D8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const MTA: usize = 0x5200;
const RAL: usize = 0x5400;
const RAH: usize = 0x5404;

// CTRL bits
const CTRL_SLU: u32 = 0x0000_0040; // set link up
const CTRL_RST: u32 = 0x0400_0000; // device reset (self-clearing)
// RCTL bits
const RCTL_EN: u32 = 0x0000_0002;
const RCTL_UPE: u32 = 0x0000_0008; // unicast promiscuous
const RCTL_MPE: u32 = 0x0000_0010; // multicast promiscuous
const RCTL_BAM: u32 = 0x0000_8000; // broadcast accept
const RCTL_SECRC: u32 = 0x0400_0000; // strip ethernet CRC (BSIZE 2048 = bits 0)
// TCTL bits
const TCTL_EN: u32 = 0x0000_0002;
const TCTL_PSP: u32 = 0x0000_0008; // pad short packets
const TCTL_CT: u32 = 0x0000_00F0; // collision threshold = 0x0F
const TCTL_COLD: u32 = 0x0004_0000; // collision distance = 0x40 (full duplex)
// TX descriptor command/status
const TXD_EOP: u8 = 0x01;
const TXD_IFCS: u8 = 0x02;
const TXD_RS: u8 = 0x08;
// RX descriptor status
const RXD_DD: u8 = 0x01;
// Interrupt cause bits we enable
const INT_LSC: u32 = 0x0000_0004;
const INT_RXDMT0: u32 = 0x0000_0010;
const INT_RXO: u32 = 0x0000_0040;
const INT_RXT0: u32 = 0x0000_0080;

const RX_DESCS: usize = 8;
const TX_DESCS: usize = 8;
const BUF: usize = 2048; // per-buffer size (RCTL BSIZE = 2048)

#[repr(C)]
#[derive(Clone, Copy)]
struct RxDesc {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TxDesc {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

unsafe fn reg(off: usize) -> u32 {
    read_volatile((NET_MMIO as usize + off) as *const u32)
}
unsafe fn setreg(off: usize, val: u32) {
    write_volatile((NET_MMIO as usize + off) as *mut u32, val);
}

/// Allocate one DMA page: map it at the next NET_DMA slot, return (vaddr, phys).
fn dma_page(slot: &mut u64) -> (u64, u64) {
    let vaddr = NET_DMA + *slot * 0x1000;
    let phys = rt::sys_dma_alloc(BOOT_MEM, vaddr).expect("[net] dma_alloc failed");
    *slot += 1;
    (vaddr, phys)
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // 1. Confirm the device the capability names, enable mem-space + bus master.
    let id = rt::sys_pci_read(BOOT_PCI, 0x00).unwrap_or(0);
    w(format!("[net] e1000 {:04x}:{:04x}\n", id & 0xFFFF, id >> 16).as_bytes());
    let cmd = rt::sys_pci_read(BOOT_PCI, 0x04).unwrap_or(0);
    let _ = rt::sys_pci_write(BOOT_PCI, 0x04, cmd | 0x6);

    // 2. Map BAR0 (the register file).
    if rt::sys_pci_bar_map(BOOT_PCI, 0, NET_MMIO).is_err() {
        w(b"[net] BAR0 map FAILED\n");
        rt::sys_exit(1);
    }

    unsafe {
        // 3. Reset: mask device interrupts, pulse CTRL.RST, wait for self-clear.
        setreg(IMC, 0xFFFF_FFFF);
        setreg(CTRL, reg(CTRL) | CTRL_RST);
        for _ in 0..1_000_000 {
            if reg(CTRL) & CTRL_RST == 0 {
                break;
            }
        }
        setreg(IMC, 0xFFFF_FFFF);
        let _ = reg(ICR); // clear any pending cause
        // Link up.
        setreg(CTRL, reg(CTRL) | CTRL_SLU);
        // Clear the multicast table (128 dwords).
        for i in 0..128 {
            setreg(MTA + i * 4, 0);
        }
    }

    // 4. DMA: descriptor rings + packet buffers. Layout (NET_DMA + n*4K):
    //    rx_ring | rx_buf x4 | tx_ring | tx_buf x4   (2 buffers per 4K page).
    let mut slot = 0u64;
    let (rx_ring_v, rx_ring_p) = dma_page(&mut slot);
    let mut rx_buf_v = [0u64; RX_DESCS];
    let mut rx_buf_p = [0u64; RX_DESCS];
    for i in (0..RX_DESCS).step_by(2) {
        let (v, p) = dma_page(&mut slot);
        rx_buf_v[i] = v;
        rx_buf_p[i] = p;
        rx_buf_v[i + 1] = v + BUF as u64;
        rx_buf_p[i + 1] = p + BUF as u64;
    }
    let (tx_ring_v, tx_ring_p) = dma_page(&mut slot);
    let mut tx_buf_v = [0u64; TX_DESCS];
    let mut tx_buf_p = [0u64; TX_DESCS];
    for i in (0..TX_DESCS).step_by(2) {
        let (v, p) = dma_page(&mut slot);
        tx_buf_v[i] = v;
        tx_buf_p[i] = p;
        tx_buf_v[i + 1] = v + BUF as u64;
        tx_buf_p[i + 1] = p + BUF as u64;
    }

    unsafe {
        // 5. RX ring: each descriptor points at a buffer; head/tail bracket the
        //    free descriptors hardware may fill ([RDH, RDT]).
        for i in 0..RX_DESCS {
            let d = (rx_ring_v as usize + i * 16) as *mut RxDesc;
            write_volatile(
                d,
                RxDesc { addr: rx_buf_p[i], length: 0, checksum: 0, status: 0, errors: 0, special: 0 },
            );
        }
        setreg(RDBAL, rx_ring_p as u32);
        setreg(RDBAH, (rx_ring_p >> 32) as u32);
        setreg(RDLEN, (RX_DESCS * 16) as u32);
        setreg(RDH, 0);
        setreg(RDT, (RX_DESCS - 1) as u32);
        setreg(RCTL, RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SECRC);

        // 6. TX ring: empty to start (head == tail).
        for i in 0..TX_DESCS {
            let d = (tx_ring_v as usize + i * 16) as *mut TxDesc;
            write_volatile(
                d,
                TxDesc { addr: 0, length: 0, cso: 0, cmd: 0, status: 0, css: 0, special: 0 },
            );
        }
        setreg(TDBAL, tx_ring_p as u32);
        setreg(TDBAH, (tx_ring_p >> 32) as u32);
        setreg(TDLEN, (TX_DESCS * 16) as u32);
        setreg(TDH, 0);
        setreg(TDT, 0);
        setreg(TIPG, 0x0060_200A); // standard inter-packet gap (copper)
        setreg(TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);
    }

    // 7. Our MAC, from the receive-address registers (loaded from EEPROM).
    let (ral, rah) = unsafe { (reg(RAL), reg(RAH)) };
    let mac = [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8, rah as u8, (rah >> 8) as u8];
    w(format!(
        "[net] MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  link STATUS {:#010x}\n",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], unsafe { reg(STATUS) }
    )
    .as_bytes());

    // 8. Bind the NIC interrupt to a notification, then enable device + PIC line.
    let notif = rt::sys_notif_create().expect("[net] notif");
    rt::sys_irq_bind(BOOT_NET_IRQ, notif).expect("[net] irq_bind");
    unsafe {
        let _ = reg(ICR); // clear
        setreg(IMS, INT_RXT0 | INT_RXO | INT_RXDMT0 | INT_LSC);
    }
    rt::sys_irq_ack(BOOT_NET_IRQ).expect("[net] irq_ack"); // arm IRQ11

    // 9. Hand-build + transmit a broadcast ARP request for the gateway.
    let frame = arp_request(&mac, [10, 0, 2, 15], [10, 0, 2, 2]);
    unsafe {
        let buf = tx_buf_v[0] as *mut u8;
        for (i, b) in frame.iter().enumerate() {
            write_volatile(buf.add(i), *b);
        }
        let d = (tx_ring_v as usize) as *mut TxDesc; // descriptor 0
        write_volatile(
            d,
            TxDesc {
                addr: tx_buf_p[0], // device DMAs from the PHYSICAL buffer address
                length: frame.len() as u16,
                cso: 0,
                cmd: TXD_EOP | TXD_IFCS | TXD_RS,
                status: 0,
                css: 0,
                special: 0,
            },
        );
        fence(Ordering::SeqCst);
        setreg(TDT, 1); // hand descriptor 0 to hardware
    }
    w(b"[net] ARP who-has 10.0.2.2 -> sent (broadcast)\n");

    // 10. Interrupt-driven receive: wait for the gateway's ARP reply.
    let mut rx_cur = 0usize;
    let mut got_reply = false;
    loop {
        let _ = rt::sys_notif_wait(notif);
        let cause = unsafe { reg(ICR) }; // reading ICR clears it + deasserts INTx
        let _ = cause;
        // Drain every descriptor hardware has marked Done.
        loop {
            let d = (rx_ring_v as usize + rx_cur * 16) as *mut RxDesc;
            let status = unsafe { read_volatile(addr_of!((*d).status)) };
            if status & RXD_DD == 0 {
                break;
            }
            let len = unsafe { read_volatile(addr_of!((*d).length)) } as usize;
            let bufv = rx_buf_v[rx_cur] as *const u8;
            handle_frame(bufv, len, &mut got_reply);
            // Recycle: clear status, hand the descriptor back via RDT.
            unsafe {
                write_volatile(addr_of_mut!((*d).status), 0);
                fence(Ordering::SeqCst);
                setreg(RDT, rx_cur as u32);
            }
            rx_cur = (rx_cur + 1) % RX_DESCS;
        }
        // Re-arm the PIC line for the next interrupt.
        let _ = rt::sys_irq_ack(BOOT_NET_IRQ);
        if got_reply {
            w(b"[net] ready (e1000 TX + RX + IRQ over capabilities)\n");
            got_reply = false; // keep parking as a resident driver
        }
    }
}

/// Inspect one received Ethernet frame; flag + report an ARP reply (the gateway).
fn handle_frame(buf: *const u8, len: usize, got_reply: &mut bool) {
    if len < 14 {
        return;
    }
    let at = |i: usize| unsafe { read_volatile(buf.add(i)) };
    let ethertype = ((at(12) as u16) << 8) | at(13) as u16;
    let src = [at(6), at(7), at(8), at(9), at(10), at(11)];
    if ethertype == 0x0806 && len >= 42 {
        let oper = ((at(20) as u16) << 8) | at(21) as u16;
        if oper == 0x0002 {
            // ARP reply: sender protocol addr at offset 28, sender hw addr at 22.
            let spa = [at(28), at(29), at(30), at(31)];
            w(format!(
                "[net] ARP reply: {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                spa[0], spa[1], spa[2], spa[3], src[0], src[1], src[2], src[3], src[4], src[5]
            )
            .as_bytes());
            *got_reply = true;
            return;
        }
    }
    w(format!("[net] rx {} bytes ethertype {:#06x}\n", len, ethertype).as_bytes());
}

/// Build a broadcast ARP request frame (Ethernet + ARP, 42 bytes).
fn arp_request(mac: &[u8; 6], spa: [u8; 4], tpa: [u8; 4]) -> [u8; 42] {
    let mut f = [0u8; 42];
    f[0..6].copy_from_slice(&[0xFF; 6]); // dst: broadcast
    f[6..12].copy_from_slice(mac); // src: us
    f[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype ARP
    f[14..16].copy_from_slice(&[0x00, 0x01]); // htype: ethernet
    f[16..18].copy_from_slice(&[0x08, 0x00]); // ptype: IPv4
    f[18] = 6; // hlen
    f[19] = 4; // plen
    f[20..22].copy_from_slice(&[0x00, 0x01]); // oper: request
    f[22..28].copy_from_slice(mac); // sender hw addr
    f[28..32].copy_from_slice(&spa); // sender protocol addr
    // target hw addr left zero
    f[38..42].copy_from_slice(&tpa); // target protocol addr
    f
}
