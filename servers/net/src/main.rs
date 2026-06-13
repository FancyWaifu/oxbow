//! net — the oxbow network stack (resident boot module).
//!
//! Layer 1 (PCI/MMIO + DMA rings + IRQ) was the e1000 arc (§19). This arc adds
//! the protocol layers from scratch — Ethernet (eth), ARP (arp), IPv4 (ipv4),
//! ICMP (icmp), and UDP (udp), plus a tiny DNS client (dns) — and proves them
//! against QEMU's SLIRP services: it ARP-resolves the gateway, sends a real DNS
//! query over UDP/IPv4 and prints the resolved address, and pings the gateway
//! over ICMP. The NIC plumbing lives in `Nic`; the higher layers are pure byte
//! shuffling over its `tx` / `recv_blocking`.
#![no_std]
#![no_main]

extern crate alloc;

mod arp;
mod dns;
mod eth;
mod icmp;
mod ipv4;
mod udp;

use alloc::format;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use eth::{ETHERTYPE_ARP, ETHERTYPE_IPV4};
use oxbow_abi::{BOOT_CONSOLE, BOOT_MEM, BOOT_NET_IRQ, BOOT_PCI, NET_DMA, NET_MMIO};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

// Our addressing (the SLIRP default network: gateway .2, DNS forwarder .3).
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];
const DNS_IP: [u8; 4] = [10, 0, 2, 3];

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

const CTRL_SLU: u32 = 0x0000_0040;
const CTRL_RST: u32 = 0x0400_0000;
const RCTL_EN: u32 = 0x0000_0002;
const RCTL_UPE: u32 = 0x0000_0008;
const RCTL_MPE: u32 = 0x0000_0010;
const RCTL_BAM: u32 = 0x0000_8000;
const RCTL_SECRC: u32 = 0x0400_0000;
const TCTL_EN: u32 = 0x0000_0002;
const TCTL_PSP: u32 = 0x0000_0008;
const TCTL_CT: u32 = 0x0000_00F0;
const TCTL_COLD: u32 = 0x0004_0000;
const TXD_EOP: u8 = 0x01;
const TXD_IFCS: u8 = 0x02;
const TXD_RS: u8 = 0x08;
const RXD_DD: u8 = 0x01;
const INT_LSC: u32 = 0x0000_0004;
const INT_RXDMT0: u32 = 0x0000_0010;
const INT_RXO: u32 = 0x0000_0040;
const INT_RXT0: u32 = 0x0000_0080;

const RX_DESCS: usize = 8;
const TX_DESCS: usize = 8;
const BUF: usize = 2048;

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

fn dma_page(slot: &mut u64) -> (u64, u64) {
    let vaddr = NET_DMA + *slot * 0x1000;
    let phys = rt::sys_dma_alloc(BOOT_MEM, vaddr).expect("[net] dma_alloc failed");
    *slot += 1;
    (vaddr, phys)
}

/// The NIC: descriptor rings, packet buffers, and the bound IRQ notification.
struct Nic {
    mac: [u8; 6],
    notif: oxbow_abi::Handle,
    rx_ring_v: u64,
    rx_buf_v: [u64; RX_DESCS],
    rx_cur: usize,
    tx_ring_v: u64,
    tx_buf_v: [u64; TX_DESCS],
    tx_buf_p: [u64; TX_DESCS],
    tx_cur: usize,
}

impl Nic {
    /// Queue a frame on the TX ring and hand it to the device.
    fn tx(&mut self, frame: &[u8]) {
        let i = self.tx_cur;
        let n = frame.len().min(BUF);
        unsafe {
            let buf = self.tx_buf_v[i] as *mut u8;
            for (k, b) in frame[..n].iter().enumerate() {
                write_volatile(buf.add(k), *b);
            }
            let d = (self.tx_ring_v as usize + i * 16) as *mut TxDesc;
            write_volatile(
                d,
                TxDesc {
                    addr: self.tx_buf_p[i],
                    length: n as u16,
                    cso: 0,
                    cmd: TXD_EOP | TXD_IFCS | TXD_RS,
                    status: 0,
                    css: 0,
                    special: 0,
                },
            );
            fence(Ordering::SeqCst);
            setreg(TDT, ((i + 1) % TX_DESCS) as u32);
        }
        self.tx_cur = (i + 1) % TX_DESCS;
    }

    /// Return the next received frame (copied into `out`), blocking on the NIC
    /// interrupt when the ring is empty.
    fn recv_blocking(&mut self, out: &mut [u8]) -> usize {
        loop {
            let d = (self.rx_ring_v as usize + self.rx_cur * 16) as *mut RxDesc;
            let status = unsafe { read_volatile(addr_of!((*d).status)) };
            if status & RXD_DD != 0 {
                let len = unsafe { read_volatile(addr_of!((*d).length)) } as usize;
                let n = len.min(out.len());
                let src = self.rx_buf_v[self.rx_cur] as *const u8;
                for (k, slot) in out[..n].iter_mut().enumerate() {
                    *slot = unsafe { read_volatile(src.add(k)) };
                }
                unsafe {
                    write_volatile(addr_of_mut!((*d).status), 0);
                    fence(Ordering::SeqCst);
                    setreg(RDT, self.rx_cur as u32);
                }
                self.rx_cur = (self.rx_cur + 1) % RX_DESCS;
                return n;
            }
            // Ring empty: park until the NIC raises IRQ11, then re-arm.
            let _ = rt::sys_notif_wait(self.notif);
            unsafe {
                let _ = reg(ICR); // reading ICR deasserts the level-triggered line
            }
            let _ = rt::sys_irq_ack(BOOT_NET_IRQ);
        }
    }
}

/// Be a good L2/L3 citizen for any frame we see: cache the sender's ARP binding,
/// answer ARP requests for our address, and echo-reply pings aimed at us.
fn handle_background(nic: &mut Nic, cache: &mut arp::Cache, frame: &[u8]) {
    let Some((_, src_mac, et, off)) = eth::parse(frame) else {
        return;
    };
    if et == ETHERTYPE_ARP {
        if let Some(a) = arp::parse(&frame[off..]) {
            cache.insert(a.spa, a.sha);
            if a.op == arp::OP_REQUEST && a.tpa == OUR_IP {
                let pkt = arp::packet(arp::OP_REPLY, nic.mac, OUR_IP, a.sha, a.spa);
                let f = eth::frame(a.sha, nic.mac, ETHERTYPE_ARP, &pkt);
                nic.tx(&f);
            }
        }
    } else if et == ETHERTYPE_IPV4 {
        if let Some(ip) = ipv4::parse(&frame[off..]) {
            cache.insert(ip.src, src_mac);
            if ip.proto == ipv4::PROTO_ICMP && ip.dst == OUR_IP {
                if let Some(e) = icmp::parse(&frame[off + ip.payload_off..]) {
                    if e.typ == icmp::ECHO_REQUEST {
                        let msg = icmp::echo(icmp::ECHO_REPLY, e.id, e.seq, &[]);
                        let pkt = ipv4::packet(OUR_IP, ip.src, ipv4::PROTO_ICMP, &msg);
                        let f = eth::frame(src_mac, nic.mac, ETHERTYPE_IPV4, &pkt);
                        nic.tx(&f);
                    }
                }
            }
        }
    }
}

/// Resolve `target` to a MAC: reply from cache, else ARP-request and wait.
fn arp_resolve(nic: &mut Nic, cache: &mut arp::Cache, target: [u8; 4]) -> [u8; 6] {
    if let Some(mac) = cache.lookup(target) {
        return mac;
    }
    let pkt = arp::packet(arp::OP_REQUEST, nic.mac, OUR_IP, [0; 6], target);
    let req = eth::frame(eth::BROADCAST, nic.mac, ETHERTYPE_ARP, &pkt);
    nic.tx(&req);
    let mut buf = [0u8; BUF];
    loop {
        let n = nic.recv_blocking(&mut buf);
        handle_background(nic, cache, &buf[..n]);
        if let Some(mac) = cache.lookup(target) {
            return mac;
        }
    }
}

/// Send a DNS A-query for `name` to the SLIRP forwarder and return the answer.
fn dns_query(nic: &mut Nic, cache: &mut arp::Cache, name: &str) -> Option<[u8; 4]> {
    let dns_mac = arp_resolve(nic, cache, DNS_IP);
    let sport: u16 = 0xC000;
    let q = dns::query(0x1234, name);
    let seg = udp::segment(OUR_IP, DNS_IP, sport, 53, &q);
    let ip = ipv4::packet(OUR_IP, DNS_IP, ipv4::PROTO_UDP, &seg);
    nic.tx(&eth::frame(dns_mac, nic.mac, ETHERTYPE_IPV4, &ip));

    let mut buf = [0u8; BUF];
    loop {
        let n = nic.recv_blocking(&mut buf);
        handle_background(nic, cache, &buf[..n]);
        let Some((_, _, et, off)) = eth::parse(&buf[..n]) else { continue };
        if et != ETHERTYPE_IPV4 {
            continue;
        }
        let Some(ip) = ipv4::parse(&buf[off..n]) else { continue };
        if ip.proto != ipv4::PROTO_UDP {
            continue;
        }
        let uoff = off + ip.payload_off;
        let Some(u) = udp::parse(&buf[uoff..n]) else { continue };
        if u.dst_port == sport {
            return dns::first_a(&buf[uoff + u.payload_off..n]);
        }
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // 1. Confirm the device, enable mem-space + bus master, map BAR0.
    let id = rt::sys_pci_read(BOOT_PCI, 0x00).unwrap_or(0);
    w(format!("[net] e1000 {:04x}:{:04x}\n", id & 0xFFFF, id >> 16).as_bytes());
    let cmd = rt::sys_pci_read(BOOT_PCI, 0x04).unwrap_or(0);
    let _ = rt::sys_pci_write(BOOT_PCI, 0x04, cmd | 0x6);
    if rt::sys_pci_bar_map(BOOT_PCI, 0, NET_MMIO).is_err() {
        w(b"[net] BAR0 map FAILED\n");
        rt::sys_exit(1);
    }

    // 2. Reset + link up + clear the multicast table.
    unsafe {
        setreg(IMC, 0xFFFF_FFFF);
        setreg(CTRL, reg(CTRL) | CTRL_RST);
        for _ in 0..1_000_000 {
            if reg(CTRL) & CTRL_RST == 0 {
                break;
            }
        }
        setreg(IMC, 0xFFFF_FFFF);
        let _ = reg(ICR);
        setreg(CTRL, reg(CTRL) | CTRL_SLU);
        for i in 0..128 {
            setreg(MTA + i * 4, 0);
        }
    }

    // 3. DMA rings + buffers (NET_DMA + n*4K): rx_ring | rx_buf×4 | tx_ring | tx_buf×4.
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
        setreg(TIPG, 0x0060_200A);
        setreg(TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);
    }

    let (ral, rah) = unsafe { (reg(RAL), reg(RAH)) };
    let mac = [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8, rah as u8, (rah >> 8) as u8];
    w(format!(
        "[net] e1000 up — MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  IP 10.0.2.15  STATUS {:#x}\n",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], unsafe { reg(STATUS) }
    )
    .as_bytes());

    // 4. Bind IRQ -> notification; enable RX interrupts; arm the PIC line.
    let notif = rt::sys_notif_create().expect("[net] notif");
    rt::sys_irq_bind(BOOT_NET_IRQ, notif).expect("[net] irq_bind");
    unsafe {
        let _ = reg(ICR);
        setreg(IMS, INT_RXT0 | INT_RXO | INT_RXDMT0 | INT_LSC);
    }
    rt::sys_irq_ack(BOOT_NET_IRQ).expect("[net] irq_ack");

    let mut nic = Nic {
        mac,
        notif,
        rx_ring_v,
        rx_buf_v,
        rx_cur: 0,
        tx_ring_v,
        tx_buf_v,
        tx_buf_p,
        tx_cur: 0,
    };
    let mut cache = arp::Cache::new();

    // 5. Demos against SLIRP: ARP, then a real DNS-over-UDP lookup, then ping.
    let gw = arp_resolve(&mut nic, &mut cache, GW_IP);
    w(format!(
        "[net] ARP: 10.0.2.2 is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        gw[0], gw[1], gw[2], gw[3], gw[4], gw[5]
    )
    .as_bytes());

    match dns_query(&mut nic, &mut cache, "example.com") {
        Some(ip) => w(format!(
            "[net] DNS: example.com -> {}.{}.{}.{}  (UDP/IPv4 over the stack)\n",
            ip[0], ip[1], ip[2], ip[3]
        )
        .as_bytes()),
        None => w(b"[net] DNS: no A record\n"),
    }

    // ICMP echo to the gateway (best-effort: SLIRP ICMP depends on host policy).
    let gw_mac = arp_resolve(&mut nic, &mut cache, GW_IP);
    let echo = icmp::echo(icmp::ECHO_REQUEST, 0x1234, 1, b"oxbow-ping");
    let ip = ipv4::packet(OUR_IP, GW_IP, ipv4::PROTO_ICMP, &echo);
    nic.tx(&eth::frame(gw_mac, nic.mac, ETHERTYPE_IPV4, &ip));
    w(b"[net] ICMP echo -> 10.0.2.2 (ping sent)\n");
    w(b"[net] ready (Ethernet/ARP/IPv4/ICMP/UDP)\n");

    // 6. Steady state: serve the network forever — cache ARP, answer ARP/ping
    //    for us, and report ICMP echo replies + other inbound traffic.
    let mut buf = [0u8; BUF];
    loop {
        let n = nic.recv_blocking(&mut buf);
        handle_background(&mut nic, &mut cache, &buf[..n]);
        if let Some((_, src, et, off)) = eth::parse(&buf[..n]) {
            if et == ETHERTYPE_IPV4 {
                if let Some(ip) = ipv4::parse(&buf[off..n]) {
                    if ip.proto == ipv4::PROTO_ICMP && ip.dst == OUR_IP {
                        if let Some(e) = icmp::parse(&buf[off + ip.payload_off..n]) {
                            if e.typ == icmp::ECHO_REPLY {
                                w(format!(
                                    "[net] ICMP echo reply from {}.{}.{}.{} seq {} via {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                                    ip.src[0], ip.src[1], ip.src[2], ip.src[3], e.seq,
                                    src[0], src[1], src[2], src[3], src[4], src[5]
                                )
                                .as_bytes());
                            }
                        }
                    }
                }
            }
        }
    }
}
