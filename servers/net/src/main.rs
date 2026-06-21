//! net — the oxbow network stack + UDP socket server (resident boot module).
//!
//! The NIC plumbing (PCI/MMIO + DMA rings + IRQ, §19) lives in `Nic`; the
//! protocol layers (eth/arp/ipv4/icmp/udp, §20) are pure byte-shuffling over its
//! `tx` / `recv_blocking`. At boot net leases an address via DHCP (§22, the
//! `dhcp` module), ARP-resolves the gateway, then serves the UDP **socket
//! capability** API (§21): clients bind sockets (each a fresh badged endpoint)
//! and send/recv datagrams through them. NIC I/O happens synchronously inside
//! request handling, sidestepping the single-thread-per-process select problem.
#![no_std]
#![no_main]

extern crate alloc;

mod arp;
mod dhcp;
mod eth;
mod icmp;
mod ipv4;
mod tcp;
mod udp;

use alloc::format;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use eth::{ETHERTYPE_ARP, ETHERTYPE_IPV4};
use oxbow_abi::{
    MsgBuf, MSG_DATA_WORDS, BOOT_CONSOLE, BOOT_EP, BOOT_MEM, BOOT_NET_IRQ, BOOT_PCI, NET_DMA, NET_MMIO, NET_SHARED,
    PROT_READ, PROT_WRITE, R_GRANT, R_SEND, TAG_NET_DNS, TAG_TCP_ACCEPT, TAG_TCP_CLOSE,
    TAG_TCP_CONNECT, TAG_TCP_CONNECT6, TAG_TCP_LISTEN,
    TAG_TCP_RECV, TAG_TCP_SEND, TAG_UDP_ATTACH, TAG_UDP_BIND, TAG_UDP_CLOSE, TAG_UDP_RECVFROM,
    TAG_UDP_RECVV, TAG_UDP_SENDTO, TAG_UDP_SENDV,
};
use oxbow_rt as rt;
use smoltcp::iface::SocketHandle;

const MAX_SOCKETS: usize = 16;

/// A socket-table slot, identified by its badge (= index + 1).
#[derive(Clone, Copy)]
enum Sock {
    Free,
    Udp(u16),          // bound port
    Tcp(SocketHandle), // smoltcp socket
    TcpListen(u16),    // a listening port (backlog lives in the TcpStack)
}

/// Server-side vaddr of socket `sid`'s shared transfer frame (netmap Stage 2):
/// each UDP socket gets its own MTU-sized frame at a distinct page so no two
/// clients ever share one. 16 sockets * 4 KiB = 64 KiB, well inside the gap
/// between NET_DMA (0x4010_0000) and the next region.
fn frame_vaddr(sid: usize) -> u64 {
    NET_SHARED + (sid as u64) * 4096
}

/// Resolve a 1-based socket badge to its table slot (copied out).
fn slot_of(sockets: &[Sock; MAX_SOCKETS], sid: usize) -> Option<Sock> {
    if (1..=MAX_SOCKETS).contains(&sid) {
        Some(sockets[sid - 1])
    } else {
        None
    }
}

/// Choose the next-hop IP: an on-link host (same /24 as us) is itself, else the
/// gateway. Works on any subnet — SLIRP's 10.0.2.0/24 or a real LAN.
fn route(ip: [u8; 4], our_ip: [u8; 4], gw: [u8; 4]) -> [u8; 4] {
    if ip[0] == our_ip[0] && ip[1] == our_ip[1] && ip[2] == our_ip[2] {
        ip
    } else {
        gw
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
    let next = route(dst_ip, nic.our_ip, nic.gw_ip);
    let mac = arp_resolve(nic, cache, next);
    let seg = udp::segment(nic.our_ip, dst_ip, src_port, dst_port, payload);
    let ip = ipv4::packet(nic.our_ip, dst_ip, ipv4::PROTO_UDP, &seg);
    nic.tx(&eth::frame(mac, nic.mac, ETHERTYPE_IPV4, &ip));
}

/// Block (serving background traffic) until a UDP datagram for `port` arrives;
/// copy its payload into `out` and return the length.
/// NON-BLOCKING UDP receive: drain whatever the NIC ring currently holds,
/// processing background traffic (ARP/ICMP), and return the first datagram for
/// `port` (or 0 if none is buffered right now). The client polls with its own
/// deadline — so a lost reply no longer blocks the server (or the caller) forever,
/// and c-ares gets the non-blocking semantics it expects.
fn recv_udp_for(
    nic: &mut Nic,
    cache: &mut arp::Cache,
    port: u16,
    out: &mut [u8],
) -> (usize, [u8; 4], u16) {
    let mut buf = [0u8; BUF];
    while let Some(n) = nic.recv_poll(&mut buf) {
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
            return (len, ip.src, u.src_port);
        }
    }
    (0, [0; 4], 0)
}

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Broadcast a DHCP message (UDP 68->67 from 0.0.0.0 to 255.255.255.255).
fn dhcp_send(nic: &mut Nic, payload: &[u8]) {
    let seg = udp::segment([0; 4], [255; 4], 68, 67, payload);
    let ip = ipv4::packet([0; 4], [255; 4], ipv4::PROTO_UDP, &seg);
    nic.tx(&eth::frame(eth::BROADCAST, nic.mac, ETHERTYPE_IPV4, &ip));
}

/// Wait for a DHCP reply of `want_type` matching `xid` (servicing background
/// traffic). Bounded so a non-answering network can't wedge boot forever.
fn dhcp_recv(
    nic: &mut Nic,
    cache: &mut arp::Cache,
    xid: u32,
    want_type: u8,
    timeout_ms: u64,
) -> Option<dhcp::Reply> {
    let mut buf = [0u8; BUF];
    // Bound by TIME via NON-BLOCKING polls: a blocking read parks on an empty ring (so
    // the deadline could never fire), so poll until the reply lands or `timeout_ms`
    // elapse, servicing background traffic (ARP/ICMP/NDP) meanwhile.
    let start = rt::sys_uptime_ms();
    while rt::sys_uptime_ms().wrapping_sub(start) < timeout_ms {
        let Some(n) = nic.recv_poll(&mut buf) else { continue };
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
        if u.dst_port != 68 {
            continue;
        }
        if let Some(r) = dhcp::parse(&buf[uoff + u.payload_off..n]) {
            if r.xid == xid && r.msg_type == want_type {
                return Some(r);
            }
        }
    }
    None
}

/// Run the DHCP DORA handshake: DISCOVER -> OFFER -> REQUEST -> ACK. RETRIES the
/// DISCOVER: SLIRP with `ipv6=on` brings its IPv4 DHCP server up a beat late and drops
/// the first DISCOVER, so a single shot would silently fall back to the static lease.
fn dhcp_acquire(nic: &mut Nic, cache: &mut arp::Cache) -> Option<dhcp::Reply> {
    let xid = 0x6F78_626F; // "oxbo"
    let mac = nic.mac;
    for _ in 0..8 {
        dhcp_send(nic, &dhcp::message(xid, &mac, dhcp::DISCOVER, None, None));
        let Some(offer) = dhcp_recv(nic, cache, xid, dhcp::OFFER, 1200) else { continue };
        dhcp_send(
            nic,
            &dhcp::message(xid, &mac, dhcp::REQUEST, Some(offer.yiaddr), Some(offer.server_id)),
        );
        if let Some(ack) = dhcp_recv(nic, cache, xid, dhcp::ACK, 1200) {
            return Some(ack);
        }
    }
    None
}

// The SLIRP gateway (also our DHCP server + router). Our own IP is leased via
// DHCP at boot into `Nic.our_ip` rather than asserted.
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
    our_ip: [u8; 4], // leased via DHCP at boot (0.0.0.0 until then)
    gw_ip: [u8; 4],  // default gateway (from the DHCP lease)
    dns_ip: [u8; 4], // DNS resolver (from the DHCP lease; clients query it)
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
    /// Non-blocking: return the next ready packet from the RX ring, or None if
    /// the ring is currently empty (does NOT park). Recycles the descriptor.
    fn recv_poll(&mut self, out: &mut [u8]) -> Option<usize> {
        let d = (self.rx_ring_v as usize + self.rx_cur * 16) as *mut RxDesc;
        let status = unsafe { read_volatile(addr_of!((*d).status)) };
        if status & RXD_DD == 0 {
            return None;
        }
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
        Some(n)
    }

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

    /// Non-blocking receive: return the next frame if the ring has one, else
    /// None. DMA fills the ring independent of the IRQ, so smoltcp can busy-poll
    /// this without depending on interrupt delivery (used by the TCP path).
    fn recv_nonblocking(&mut self, out: &mut [u8]) -> Option<usize> {
        let d = (self.rx_ring_v as usize + self.rx_cur * 16) as *mut RxDesc;
        let status = unsafe { read_volatile(addr_of!((*d).status)) };
        if status & RXD_DD == 0 {
            return None;
        }
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
        Some(n)
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
            if a.op == arp::OP_REQUEST && a.tpa == nic.our_ip {
                let pkt = arp::packet(arp::OP_REPLY, nic.mac, nic.our_ip, a.sha, a.spa);
                let f = eth::frame(a.sha, nic.mac, ETHERTYPE_ARP, &pkt);
                nic.tx(&f);
            }
        }
    } else if et == ETHERTYPE_IPV4 {
        if let Some(ip) = ipv4::parse(&frame[off..]) {
            cache.insert(ip.src, src_mac);
            if ip.proto == ipv4::PROTO_ICMP && ip.dst == nic.our_ip {
                if let Some(e) = icmp::parse(&frame[off + ip.payload_off..]) {
                    if e.typ == icmp::ECHO_REQUEST {
                        let msg = icmp::echo(icmp::ECHO_REPLY, e.id, e.seq, &[]);
                        let pkt = ipv4::packet(nic.our_ip, ip.src, ipv4::PROTO_ICMP, &msg);
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
    let pkt = arp::packet(arp::OP_REQUEST, nic.mac, nic.our_ip, [0; 6], target);
    let req = eth::frame(eth::BROADCAST, nic.mac, ETHERTYPE_ARP, &pkt);
    let mut buf = [0u8; BUF];
    // Time-bounded + retransmitting so a silent peer (e.g. SLIRP in IPv6-only mode, where
    // there is no IPv4 host to answer) can't wedge boot. Returns the zero MAC on give-up.
    let start = rt::sys_uptime_ms();
    let mut last_tx = 0u64;
    while rt::sys_uptime_ms().wrapping_sub(start) < 4000 {
        if rt::sys_uptime_ms().wrapping_sub(last_tx) > 500 {
            nic.tx(&req);
            last_tx = rt::sys_uptime_ms();
        }
        let Some(n) = nic.recv_poll(&mut buf) else { continue };
        handle_background(nic, cache, &buf[..n]);
        if let Some(mac) = cache.lookup(target) {
            return mac;
        }
    }
    [0; 6]
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
        "[net] e1000 up — MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  STATUS {:#x}\n",
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
        our_ip: [0; 4], // until DHCP leases one
        gw_ip: GW_IP,   // overwritten by the DHCP router option
        dns_ip: [10, 0, 2, 3], // SLIRP DNS fallback; overwritten by the lease
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

    // 5. DHCP: lease an address + gateway + DNS (DORA) instead of asserting one.
    //    Falls back to the well-known SLIRP lease if no server answers.
    match dhcp_acquire(&mut nic, &mut cache) {
        Some(l) => {
            nic.our_ip = l.yiaddr;
            if l.router != [0; 4] {
                nic.gw_ip = l.router;
            }
            if l.dns != [0; 4] {
                nic.dns_ip = l.dns;
            }
            w(format!(
                "[net] DHCP lease: IP {}.{}.{}.{}  gw {}.{}.{}.{}  dns {}.{}.{}.{}\n",
                l.yiaddr[0], l.yiaddr[1], l.yiaddr[2], l.yiaddr[3],
                nic.gw_ip[0], nic.gw_ip[1], nic.gw_ip[2], nic.gw_ip[3],
                l.dns[0], l.dns[1], l.dns[2], l.dns[3]
            )
            .as_bytes());
        }
        None => {
            nic.our_ip = [10, 0, 2, 15];
            w(b"[net] DHCP failed; using static 10.0.2.15\n");
        }
    }

    // Prove routing works: ARP-resolve the (real) gateway, populating the cache.
    let gwip = nic.gw_ip;
    let gw = arp_resolve(&mut nic, &mut cache, gwip);
    w(format!(
        "[net] gateway {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        gwip[0], gwip[1], gwip[2], gwip[3], gw[0], gw[1], gw[2], gw[3], gw[4], gw[5]
    )
    .as_bytes());

    // 6. Stand up the smoltcp TCP stack with our leased address + gateway. It
    //    shares the NIC by raw pointer (single-threaded; no aliasing in practice).
    let nic_ptr: *mut Nic = &mut nic;
    let mut tcp_stack = tcp::TcpStack::new(nic_ptr, nic.mac, nic.our_ip, nic.gw_ip);
    w(b"[net] ready (UDP + TCP socket service on the network endpoint)\n");

    // 7. Serve the socket capability API: clients bind/connect sockets (each a
    //    fresh badged endpoint, badge = socket id) and send/recv through them.
    let mut sockets = [Sock::Free; MAX_SOCKETS];
    // Per-socket shared transfer frames (netmap Stage 2). Each UDP socket id gets
    // its OWN MTU-sized frame, mapped at frame_vaddr(sid) on our side, with a COPY
    // of the cap handed to the owning client (handle transfer is a copy — §3.4 — so
    // we keep ours; there is no unmap syscall, so we map once and keep it). One frame
    // per socket id => correct isolation: two different processes never map the same
    // page. The fixed sid->frame binding means a reused socket slot re-shares the same
    // page with its new owner, so the table never leaks past 16 frames.
    let mut socket_frames: [Option<oxbow_abi::Handle>; MAX_SOCKETS + 1] =
        [None; MAX_SOCKETS + 1];
    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_EP, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut r = MsgBuf::new(0);
        match m.tag {
            // Control channel (badge = NET_CTL): report the leased DNS resolver.
            TAG_NET_DNS => {
                r.data[0] = nic.dns_ip[0] as u64;
                r.data[1] = nic.dns_ip[1] as u64;
                r.data[2] = nic.dns_ip[2] as u64;
                r.data[3] = nic.dns_ip[3] as u64;
                r.data_len = 4;
                let _ = rt::sys_reply(reply, &r);
            }
            // Socket channel (badge = socket id): hand the OWNING client a cap to
            // THIS socket's transfer frame (allocated + mapped at frame_vaddr(sid) on
            // first use). The reply COPIES the cap into the client (which maps the same
            // physical page) and returns `sid` so the client picks a stable per-sid
            // vaddr. Per-socket => no two processes ever share a page.
            TAG_UDP_ATTACH => {
                let sid = m.badge as usize;
                if (1..=MAX_SOCKETS).contains(&sid)
                    && matches!(slot_of(&sockets, sid), Some(Sock::Udp(_)))
                {
                    if socket_frames[sid].is_none() {
                        if let Ok(f) = rt::sys_frame_alloc(BOOT_MEM) {
                            if rt::sys_frame_map(f, frame_vaddr(sid), PROT_READ | PROT_WRITE).is_ok()
                            {
                                socket_frames[sid] = Some(f);
                            }
                        }
                    }
                    match socket_frames[sid] {
                        Some(f) => {
                            r.data[0] = 0;
                            r.data[1] = sid as u64;
                            r.data_len = 2;
                            r.handle_count = 1;
                            r.handles[0] = f;
                        }
                        None => {
                            r.data[0] = 1;
                            r.data_len = 1;
                        }
                    }
                } else {
                    r.data[0] = 1;
                    r.data_len = 1;
                }
                let _ = rt::sys_reply(reply, &r);
            }
            // Socket channel: send a datagram FROM this socket's frame (large path).
            TAG_UDP_SENDV => {
                let sid = m.badge as usize;
                if sid <= MAX_SOCKETS && socket_frames[sid].is_some() {
                    if let Some(Sock::Udp(src_port)) = slot_of(&sockets, sid) {
                        let dst_ip = (m.data[0] as u32).to_be_bytes();
                        let dport = m.data[1] as u16;
                        let len = (m.data[2] as usize).min(1472);
                        let payload: Vec<u8> = unsafe {
                            core::slice::from_raw_parts(frame_vaddr(sid) as *const u8, len)
                        }
                        .to_vec();
                        send_udp(&mut nic, &mut cache, src_port, dst_ip, dport, &payload);
                        r.data[0] = 0;
                    } else {
                        r.data[0] = 1;
                    }
                } else {
                    r.data[0] = 1;
                }
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            // Socket channel: receive a datagram INTO this socket's frame (large path).
            // Returns length + the sender's IPv4/port (data[1]/data[2]) so the client
            // shim can serve std's recv_from with no truncation up to the full MTU.
            TAG_UDP_RECVV => {
                let sid = m.badge as usize;
                if sid <= MAX_SOCKETS && socket_frames[sid].is_some() {
                    if let Some(Sock::Udp(port)) = slot_of(&sockets, sid) {
                        let out = unsafe {
                            core::slice::from_raw_parts_mut(frame_vaddr(sid) as *mut u8, 1472)
                        };
                        let (n, sip, sport) = recv_udp_for(&mut nic, &mut cache, port, out);
                        r.data[0] = n as u64;
                        r.data[1] = u32::from_be_bytes(sip) as u64;
                        r.data[2] = sport as u64;
                    } else {
                        r.data[0] = 0;
                    }
                } else {
                    r.data[0] = 0;
                }
                r.data_len = 3;
                let _ = rt::sys_reply(reply, &r);
            }
            // Socket channel: free the UDP socket slot (else binds leak slots).
            TAG_UDP_CLOSE => {
                let sid = m.badge as usize;
                if (1..=MAX_SOCKETS).contains(&sid) {
                    if let Sock::Udp(_) = sockets[sid - 1] {
                        sockets[sid - 1] = Sock::Free;
                    }
                }
                r.data[0] = 0;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            // Control channel (badge = NET_CTL): allocate a socket + mint its cap.
            TAG_UDP_BIND => {
                let req_port = m.data[0] as u16;
                match sockets.iter().position(|s| matches!(s, Sock::Free)) {
                    Some(idx) => {
                        let port = if req_port == 0 { 0xC000 + idx as u16 } else { req_port };
                        sockets[idx] = Sock::Udp(port);
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
                if let Some(Sock::Udp(src_port)) = slot_of(&sockets, sid) {
                    let dst_ip = (m.data[0] as u32).to_be_bytes();
                    let dport = m.data[1] as u16;
                    // Payload rides the inline area past the 24-byte addr/port/len header.
                    let len = (m.data[2] as usize).min(480);
                    let bytes =
                        unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(24), len) };
                    let payload: Vec<u8> = bytes.to_vec();
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
                if let Some(Sock::Udp(port)) = slot_of(&sockets, sid) {
                    // Up to 480 payload bytes inline (was 56 — which silently TRUNCATED
                    // any datagram > 56 B on receive). Payload at byte 8; the sender
                    // address moves to the LAST two words so it never collides with the
                    // bigger payload window.
                    let mut out = [0u8; 480];
                    let (n, sip, sport) = recv_udp_for(&mut nic, &mut cache, port, &mut out);
                    r.data[0] = n as u64;
                    let dst = r.data.as_mut_ptr() as *mut u8;
                    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), dst.add(8), n) };
                    // §101 sender address (for std recv_from) at the high words, clear of
                    // the payload: src IPv4 (BE) at data[62], src port at data[63].
                    r.data[MSG_DATA_WORDS - 2] = u32::from_be_bytes(sip) as u64;
                    r.data[MSG_DATA_WORDS - 1] = sport as u64;
                    r.data_len = MSG_DATA_WORDS as u32;
                } else {
                    r.data[0] = 0;
                    r.data_len = 1;
                }
                let _ = rt::sys_reply(reply, &r);
            }
            // Control channel: open a TCP connection, mint a socket cap on success.
            TAG_TCP_CONNECT => {
                let dst_ip = (m.data[0] as u32).to_be_bytes();
                let dport = m.data[1] as u16;
                let free = sockets.iter().position(|s| matches!(s, Sock::Free));
                match free.and_then(|idx| tcp_stack.connect(dst_ip, dport).map(|h| (idx, h))) {
                    Some((idx, handle)) => {
                        sockets[idx] = Sock::Tcp(handle);
                        match rt::sys_mint(BOOT_EP, (idx + 1) as u64, R_SEND | R_GRANT) {
                            Ok(cap) => {
                                r.data[0] = 0;
                                r.data_len = 1;
                                r.handle_count = 1;
                                r.handles[0] = cap;
                                let _ = rt::sys_reply(reply, &r);
                                let _ = rt::sys_close(cap);
                            }
                            Err(_) => {
                                tcp_stack.close(handle);
                                sockets[idx] = Sock::Free;
                                r.data[0] = 1;
                                r.data_len = 1;
                                let _ = rt::sys_reply(reply, &r);
                            }
                        }
                    }
                    None => {
                        r.data[0] = 1; // no slot, or connection refused/timed out
                        r.data_len = 1;
                        let _ = rt::sys_reply(reply, &r);
                    }
                }
            }
            // Control channel: open an IPv6 TCP connection. data[0]=port, 16 addr bytes
            // at byte offset 8.
            TAG_TCP_CONNECT6 => {
                let dport = m.data[0] as u16;
                let mut addr = [0u8; 16];
                let src =
                    unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), 16) };
                addr.copy_from_slice(src);
                let free = sockets.iter().position(|s| matches!(s, Sock::Free));
                match free.and_then(|idx| tcp_stack.connect6(addr, dport).map(|h| (idx, h))) {
                    Some((idx, handle)) => {
                        sockets[idx] = Sock::Tcp(handle);
                        match rt::sys_mint(BOOT_EP, (idx + 1) as u64, R_SEND | R_GRANT) {
                            Ok(cap) => {
                                r.data[0] = 0;
                                r.data_len = 1;
                                r.handle_count = 1;
                                r.handles[0] = cap;
                                let _ = rt::sys_reply(reply, &r);
                                let _ = rt::sys_close(cap);
                            }
                            Err(_) => {
                                tcp_stack.close(handle);
                                sockets[idx] = Sock::Free;
                                r.data[0] = 1;
                                r.data_len = 1;
                                let _ = rt::sys_reply(reply, &r);
                            }
                        }
                    }
                    None => {
                        r.data[0] = 1;
                        r.data_len = 1;
                        let _ = rt::sys_reply(reply, &r);
                    }
                }
            }
            // TCP socket channel: send bytes.
            TAG_TCP_SEND => {
                let sid = m.badge as usize;
                if let Some(Sock::Tcp(handle)) = slot_of(&sockets, sid) {
                    // Up to 504 payload bytes ride the inline data area (512 B - 8 B count).
                    let len = (m.data[0] as usize).min(504);
                    let bytes =
                        unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(8), len) };
                    match tcp_stack.send(handle, bytes) {
                        Some(sent) => {
                            r.data[0] = 0; // ok
                            r.data[1] = sent as u64; // bytes actually accepted (may be < len)
                        }
                        None => {
                            r.data[0] = 1; // socket can't send
                            r.data[1] = 0;
                        }
                    }
                } else {
                    r.data[0] = 1;
                    r.data[1] = 0;
                }
                r.data_len = 2;
                let _ = rt::sys_reply(reply, &r);
            }
            // TCP socket channel: receive bytes (blocks until data or close).
            TAG_TCP_RECV => {
                let sid = m.badge as usize;
                // Consume only as many bytes as the client can take this call;
                // smoltcp keeps the rest buffered for the next recv (byte-exact,
                // which TLS requires).
                let want = (m.data[0] as usize).clamp(1, 504);
                if let Some(Sock::Tcp(handle)) = slot_of(&sockets, sid) {
                    let mut out = [0u8; 504];
                    let n = tcp_stack.recv(handle, &mut out[..want]);
                    r.data[0] = n as u64;
                    let dst = r.data.as_mut_ptr() as *mut u8;
                    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), dst.add(8), n) };
                    r.data_len = 64;
                } else {
                    r.data[0] = 0;
                    r.data_len = 1;
                }
                let _ = rt::sys_reply(reply, &r);
            }
            // TCP socket channel: close + free the slot (a socket or a listener).
            TAG_TCP_CLOSE => {
                let sid = m.badge as usize;
                match slot_of(&sockets, sid) {
                    Some(Sock::Tcp(handle)) => {
                        tcp_stack.close(handle);
                        sockets[sid - 1] = Sock::Free;
                    }
                    Some(Sock::TcpListen(_)) => {
                        // Free the listener slot; its backlog sockets stay in the stack.
                        sockets[sid - 1] = Sock::Free;
                    }
                    _ => {}
                }
                r.data[0] = 0;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            // Control channel: start listening on a port; mint a badged listener cap.
            TAG_TCP_LISTEN => {
                let port = m.data[0] as u16;
                let free = sockets.iter().position(|s| matches!(s, Sock::Free));
                match free {
                    Some(idx) if tcp_stack.listen(port, 4) => {
                        sockets[idx] = Sock::TcpListen(port);
                        match rt::sys_mint(BOOT_EP, (idx + 1) as u64, R_SEND | R_GRANT) {
                            Ok(cap) => {
                                r.data[0] = 0;
                                r.data_len = 1;
                                r.handle_count = 1;
                                r.handles[0] = cap;
                                let _ = rt::sys_reply(reply, &r);
                                let _ = rt::sys_close(cap);
                            }
                            Err(_) => {
                                sockets[idx] = Sock::Free;
                                r.data[0] = 1;
                                r.data_len = 1;
                                let _ = rt::sys_reply(reply, &r);
                            }
                        }
                    }
                    _ => {
                        r.data[0] = 1;
                        r.data_len = 1;
                        let _ = rt::sys_reply(reply, &r);
                    }
                }
            }
            // Listener channel: non-blocking accept — the client polls. On a pending
            // connection, mint a socket cap and report the peer address.
            TAG_TCP_ACCEPT => {
                let sid = m.badge as usize;
                let mut replied = false;
                if let Some(Sock::TcpListen(port)) = slot_of(&sockets, sid) {
                    if let Some((handle, addr, is_v6, peer_port)) = tcp_stack.accept(port) {
                        match sockets.iter().position(|s| matches!(s, Sock::Free)) {
                            Some(idx) => {
                                sockets[idx] = Sock::Tcp(handle);
                                if let Ok(cap) = rt::sys_mint(BOOT_EP, (idx + 1) as u64, R_SEND | R_GRANT)
                                {
                                    // data[0]=status, [1]=family(4/6), [2]=port, 16 peer
                                    // address bytes at byte offset 24 (data[3..5]).
                                    r.data[0] = 0;
                                    r.data[1] = if is_v6 { 6 } else { 4 };
                                    r.data[2] = peer_port as u64;
                                    let dst = r.data.as_mut_ptr() as *mut u8;
                                    unsafe { core::ptr::copy_nonoverlapping(addr.as_ptr(), dst.add(24), 16) };
                                    r.data_len = 5;
                                    r.handle_count = 1;
                                    r.handles[0] = cap;
                                    let _ = rt::sys_reply(reply, &r);
                                    let _ = rt::sys_close(cap);
                                    replied = true;
                                } else {
                                    tcp_stack.close(handle);
                                    sockets[idx] = Sock::Free;
                                }
                            }
                            None => tcp_stack.close(handle), // no free slot — drop it
                        }
                    }
                }
                if !replied {
                    r.data[0] = 1; // nothing pending yet (or not a listener)
                    r.data_len = 1;
                    let _ = rt::sys_reply(reply, &r);
                }
            }
            _ => {
                r.data[0] = 1;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
        }
    }
}
