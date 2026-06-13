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
mod eth;
mod icmp;
mod ipv4;
mod udp;

use alloc::format;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use eth::{ETHERTYPE_ARP, ETHERTYPE_IPV4};
use oxbow_abi::{
    MsgBuf, BOOT_CONSOLE, BOOT_EP, BOOT_MEM, BOOT_NET_IRQ, BOOT_PCI, NET_DMA, NET_MMIO, R_GRANT,
    R_SEND, TAG_UDP_BIND, TAG_UDP_RECVFROM, TAG_UDP_SENDTO,
};
use oxbow_rt as rt;

const MAX_SOCKETS: usize = 8;

#[derive(Clone, Copy)]
struct Socket {
    in_use: bool,
    port: u16,
}

/// Choose the next-hop MAC target: a 10.0.2.0/24 host is on-link, else the gateway.
fn route(ip: [u8; 4]) -> [u8; 4] {
    if ip[0] == 10 && ip[1] == 0 && ip[2] == 2 {
        ip
    } else {
        GW_IP
    }
}

/// Send a UDP datagram (resolving the next-hop MAC, building IPv4 + Ethernet).
fn send_udp(
    nic: &mut Nic,
    cache: &mut arp::Cache,
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    payload: &[u8],
) {
    let mac = arp_resolve(nic, cache, route(dst_ip));
    let seg = udp::segment(OUR_IP, dst_ip, src_port, dst_port, payload);
    let ip = ipv4::packet(OUR_IP, dst_ip, ipv4::PROTO_UDP, &seg);
    nic.tx(&eth::frame(mac, nic.mac, ETHERTYPE_IPV4, &ip));
}

/// Block (serving background traffic) until a UDP datagram for `port` arrives;
/// copy its payload into `out` and return the length.
fn recv_udp_for(nic: &mut Nic, cache: &mut arp::Cache, port: u16, out: &mut [u8]) -> usize {
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
        if u.dst_port == port {
            let p = &buf[uoff + u.payload_off..n];
            let len = p.len().min(out.len());
            out[..len].copy_from_slice(&p[..len]);
            return len;
        }
    }
}

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

// Our addressing (the SLIRP default network: gateway .2, DNS forwarder .3).
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

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
            // Ring empty: re-arm the line FIRST (we may have drained the ring
            // without ever parking, leaving IRQ11 masked from its last fire),
            // then park until the NIC raises it again.
            unsafe {
                let _ = reg(ICR); // reading ICR deasserts the level-triggered line
            }
            let _ = rt::sys_irq_ack(BOOT_NET_IRQ);
            let _ = rt::sys_notif_wait(self.notif);
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

    // 5. Prove the NIC + stack work: ARP-resolve the gateway (populates cache).
    let gw = arp_resolve(&mut nic, &mut cache, GW_IP);
    w(format!(
        "[net] ARP: 10.0.2.2 is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        gw[0], gw[1], gw[2], gw[3], gw[4], gw[5]
    )
    .as_bytes());
    w(b"[net] ready (UDP socket service on the network endpoint)\n");

    // 6. Serve the socket capability API: clients bind UDP sockets (each a fresh
    //    badged endpoint, badge = socket id) and send/recv datagrams through
    //    them. The badge makes the server stateless beyond a tiny port table.
    let mut sockets = [Socket { in_use: false, port: 0 }; MAX_SOCKETS];
    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_EP, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut r = MsgBuf::new(0);
        match m.tag {
            // Control channel (badge = NET_CTL): allocate a socket + mint its cap.
            TAG_UDP_BIND => {
                let req_port = m.data[0] as u16;
                match sockets.iter().position(|s| !s.in_use) {
                    Some(idx) => {
                        let port = if req_port == 0 { 0xC000 + idx as u16 } else { req_port };
                        sockets[idx] = Socket { in_use: true, port };
                        match rt::sys_mint(BOOT_EP, (idx + 1) as u64, R_SEND | R_GRANT) {
                            Ok(cap) => {
                                r.data[0] = 0;
                                r.data[1] = port as u64;
                                r.data_len = 2;
                                r.handle_count = 1;
                                r.handles[0] = cap;
                                let _ = rt::sys_reply(reply, &r);
                                let _ = rt::sys_close(cap);
                            }
                            Err(_) => {
                                r.data[0] = 1;
                                r.data_len = 1;
                                let _ = rt::sys_reply(reply, &r);
                            }
                        }
                    }
                    None => {
                        r.data[0] = 1; // no free socket
                        r.data_len = 1;
                        let _ = rt::sys_reply(reply, &r);
                    }
                }
            }
            // Socket channel (badge = socket id): send a datagram.
            TAG_UDP_SENDTO => {
                let sid = m.badge as usize;
                if sid >= 1 && sid <= MAX_SOCKETS && sockets[sid - 1].in_use {
                    let dst_ip = (m.data[0] as u32).to_be_bytes();
                    let dport = m.data[1] as u16;
                    let len = (m.data[2] as usize).min(40);
                    let bytes =
                        unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(24), len) };
                    let payload: Vec<u8> = bytes.to_vec();
                    let src_port = sockets[sid - 1].port;
                    send_udp(&mut nic, &mut cache, src_port, dst_ip, dport, &payload);
                    r.data[0] = 0;
                } else {
                    r.data[0] = 1;
                }
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            // Socket channel: receive a datagram (blocks until one arrives).
            TAG_UDP_RECVFROM => {
                let sid = m.badge as usize;
                if sid >= 1 && sid <= MAX_SOCKETS && sockets[sid - 1].in_use {
                    let port = sockets[sid - 1].port;
                    let mut out = [0u8; 56];
                    let n = recv_udp_for(&mut nic, &mut cache, port, &mut out);
                    r.data[0] = n as u64;
                    let dst = r.data.as_mut_ptr() as *mut u8;
                    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), dst.add(8), n) };
                    r.data_len = 8;
                } else {
                    r.data[0] = 0;
                    r.data_len = 1;
                }
                let _ = rt::sys_reply(reply, &r);
            }
            _ => {
                r.data[0] = 1;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
        }
    }
}
