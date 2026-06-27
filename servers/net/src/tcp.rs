//! TCP via smoltcp (§23). smoltcp is the TCP/IP state machine; oxbow provides
//! the layers below (the e1000 driver as a smoltcp `phy::Device`) and above (the
//! socket-capability glue). Each TCP operation drives `Interface::poll` in a
//! busy-loop with a real uptime deadline — DMA fills the RX ring regardless of
//! the IRQ, so polling needs no interrupt, sidestepping oxbow's
//! single-thread-per-process "wait on the NIC and the timer at once" problem.
use crate::Nic;
use alloc::vec;
use oxbow_rt as rt;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Loopback, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv6Address,
};

fn now() -> Instant {
    Instant::from_millis(rt::sys_uptime_ms() as i64)
}

/// The Modified EUI-64 interface identifier for a MAC (insert ff:fe, flip the U/L bit).
fn eui64(mac: [u8; 6]) -> [u8; 8] {
    [mac[0] ^ 0x02, mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]]
}

// --- the e1000 as a smoltcp phy::Device ------------------------------------
// Raw pointer to the NIC: `receive` must hand back both an RxToken and a TxToken,
// which would otherwise need two &mut borrows of the device. We are single
// threaded and use the tokens sequentially, so a pointer is sound here.
//
// The tokens hold FIXED buffers — no heap. This is essential: the connect/recv
// busy-poll loops call `receive` thousands of times, and oxbow's bump allocator
// never frees (§17), so a per-poll Vec would exhaust the budget in milliseconds.
const FRAME_CAP: usize = 2048; // >= MTU + headers

pub struct PhyDevice {
    nic: *mut Nic,
}
pub struct PhyRx {
    buf: [u8; FRAME_CAP],
    len: usize,
}
pub struct PhyTx {
    nic: *mut Nic,
}

impl Device for PhyDevice {
    type RxToken<'a> = PhyRx;
    type TxToken<'a> = PhyTx;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.max_transmission_unit = 1500;
        c.medium = Medium::Ethernet;
        c
    }

    fn receive(&mut self, _t: Instant) -> Option<(PhyRx, PhyTx)> {
        let nic = unsafe { &mut *self.nic };
        let mut buf = [0u8; FRAME_CAP];
        // Drain the async pump's software RX queue first (TCP frames it pulled off the
        // NIC for us), then the NIC ring. The queue is empty outside async-RX mode, so
        // there this stays the unchanged direct-from-NIC path.
        let got = nic.pop_rx_tcp(&mut buf).or_else(|| nic.recv_nonblocking(&mut buf));
        got.map(|n| (PhyRx { buf, len: n }, PhyTx { nic: self.nic }))
    }

    fn transmit(&mut self, _t: Instant) -> Option<PhyTx> {
        Some(PhyTx { nic: self.nic })
    }
}

impl phy::RxToken for PhyRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.buf[..self.len])
    }
}

impl phy::TxToken for PhyTx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; FRAME_CAP];
        let n = len.min(FRAME_CAP);
        let r = f(&mut buf[..n]);
        unsafe { (*self.nic).tx(&buf[..n]) };
        r
    }
}

// --- the TCP stack ----------------------------------------------------------
pub struct TcpStack {
    device: PhyDevice,
    iface: Interface,
    /// Loopback path: a separate smoltcp `Interface` over a `Loopback` device holding
    /// 127.0.0.1/8, sharing the SocketSet. Local connects (X clients -> Xwayland) route
    /// to 127/8 here and the handshake completes on the loopback device. Polled BEFORE
    /// the e1000 each turn so a local SYN is delivered before e1000's retransmit timer
    /// could leak it to the LAN.
    lo_device: Loopback,
    lo_iface: Interface,
    sockets: SocketSet<'static>,
    next_port: u16,
    /// Sockets currently in LISTEN state, with the port they listen on. A backlog of
    /// these per port lets several incoming connections be accepted; each is removed
    /// (and replenished) when it transitions to ESTABLISHED.
    listeners: alloc::vec::Vec<(u16, SocketHandle)>,
}

impl TcpStack {
    /// Build the interface with our leased IP (/24) and the gateway as the
    /// default route. smoltcp does its own ARP, so it resolves peers itself.
    pub fn new(nic: *mut Nic, mac: [u8; 6], ip: [u8; 4], gw: [u8; 4]) -> Self {
        let mut device = PhyDevice { nic };
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        let mut iface = Interface::new(config, &mut device, now());
        // IPv6 (§104): a link-local fe80::/64 (EUI-64 from the MAC, for NDP) and a global
        // in SLIRP's default fec0::/64 prefix. smoltcp runs IPv6 + Neighbor Discovery
        // itself; we just assign the addresses and a default route via SLIRP's v6 host.
        let eui = eui64(mac);
        let mut ll = [0u8; 16];
        ll[0] = 0xfe;
        ll[1] = 0x80;
        ll[8..16].copy_from_slice(&eui);
        let mut gl = [0u8; 16];
        gl[0] = 0xfe;
        gl[1] = 0xc0;
        gl[15] = mac[5]; // fec0::<last MAC byte> — unique per host (distinct VMs differ)
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), 24));
            let _ = a.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address::from(ll)), 64));
            let _ = a.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address::from(gl)), 64));
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(gw[0], gw[1], gw[2], gw[3]));
        // SLIRP's IPv6 host/router is fec0::2.
        let mut r6 = [0u8; 16];
        r6[0] = 0xfe;
        r6[1] = 0xc0;
        r6[15] = 0x02;
        let _ = iface
            .routes_mut()
            .add_default_ipv6_route(Ipv6Address::from(r6));
        // Join the solicited-node multicast group for each address so incoming Neighbor
        // Solicitations for us are accepted (smoltcp drops packets to un-joined groups)
        // and answered with a Neighbor Advertisement — without this, peers can't resolve
        // our MAC and no IPv6 connection can complete.
        for a in [&ll, &gl] {
            let mut sn = [0u8; 16];
            sn[0] = 0xff;
            sn[1] = 0x02;
            sn[11] = 0x01;
            sn[12] = 0xff;
            sn[13] = a[13];
            sn[14] = a[14];
            sn[15] = a[15];
            let _ = iface.join_multicast_group(Ipv6Address::from(sn));
        }
        // Loopback interface: IP medium (no MAC/ARP), 127.0.0.1/8 connected.
        let mut lo_device = Loopback::new(Medium::Ip);
        let mut lo_iface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut lo_device,
            now(),
        );
        lo_iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        });
        TcpStack {
            device,
            iface,
            lo_device,
            lo_iface,
            sockets: SocketSet::new(vec![]),
            next_port: 49152,
            listeners: vec![],
        }
    }

    fn new_listening_socket(&mut self, port: u16) -> Option<SocketHandle> {
        let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let mut sock = tcp::Socket::new(rx, tx);
        if sock.listen(port).is_err() {
            return None;
        }
        Some(self.sockets.add(sock))
    }

    /// Open a `backlog` of listening sockets on `port`. Returns false if none started.
    pub fn listen(&mut self, port: u16, backlog: usize) -> bool {
        let mut any = false;
        for _ in 0..backlog {
            if let Some(h) = self.new_listening_socket(port) {
                self.listeners.push((port, h));
                any = true;
            }
        }
        any
    }

    /// Non-blocking accept: poll the stack; if a listening socket on `port` has reached
    /// ESTABLISHED, hand it back as the accepted connection with the peer address, and
    /// replenish a fresh listening socket. `None` = nothing pending this poll.
    /// Returns `(socket, peer_addr_16, is_v6, peer_port)`. For an IPv4 peer the address
    /// occupies the first 4 bytes; for IPv6 it is the full 16 bytes.
    pub fn accept(&mut self, port: u16) -> Option<(SocketHandle, [u8; 16], bool, u16)> {
        self.poll();
        let idx = self.listeners.iter().position(|&(p, h)| {
            p == port && self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established
        })?;
        let (p, h) = self.listeners.remove(idx);
        let (addr, is_v6, peer_port) = match self.sockets.get::<tcp::Socket>(h).remote_endpoint() {
            Some(ep) => {
                let mut a = [0u8; 16];
                let v6 = match ep.addr {
                    IpAddress::Ipv4(v4) => {
                        a[..4].copy_from_slice(&v4.octets());
                        false
                    }
                    IpAddress::Ipv6(v6) => {
                        a.copy_from_slice(&v6.octets());
                        true
                    }
                };
                (a, v6, ep.port)
            }
            None => ([0u8; 16], false, 0),
        };
        if let Some(nh) = self.new_listening_socket(p) {
            self.listeners.push((p, nh));
        }
        Some((h, addr, is_v6, peer_port))
    }

    fn poll(&mut self) {
        // Loopback first: local handshakes settle on the loopback device before the
        // e1000's retransmit timer could emit a 127/8 SYN onto the LAN.
        let t = now();
        let _ = self.lo_iface.poll(t, &mut self.lo_device, &mut self.sockets);
        let _ = self.iface.poll(t, &mut self.device, &mut self.sockets);
    }

    /// Open a connection to an IPv4 `dst:port`. Handle once established, else None.
    pub fn connect(&mut self, dst: [u8; 4], port: u16) -> Option<SocketHandle> {
        let remote = IpEndpoint::new(IpAddress::v4(dst[0], dst[1], dst[2], dst[3]), port);
        self.connect_endpoint(remote)
    }

    /// Open a connection to an IPv6 `dst:port`. smoltcp drives Neighbor Discovery to
    /// resolve the next hop and emits the SYN over IPv6.
    pub fn connect6(&mut self, dst: [u8; 16], port: u16) -> Option<SocketHandle> {
        let remote = IpEndpoint::new(IpAddress::Ipv6(Ipv6Address::from(dst)), port);
        self.connect_endpoint(remote)
    }

    fn connect_endpoint(&mut self, remote: IpEndpoint) -> Option<SocketHandle> {
        let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let mut sock = tcp::Socket::new(rx, tx);
        let local = self.next_port;
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        // smoltcp picks the local source address via cx.get_source_address(remote). For a
        // loopback dest we MUST use the loopback iface's context so the source is 127.0.0.1
        // (using e1000's context would pick 10.0.2.15 and the SYN-ACK would route out the LAN
        // instead of back over loopback — the handshake would never complete).
        let is_loopback = matches!(remote.addr, IpAddress::Ipv4(v4) if v4.octets()[0] == 127);
        let cx = if is_loopback { self.lo_iface.context() } else { self.iface.context() };
        if sock.connect(cx, remote, local).is_err() {
            return None;
        }
        let handle = self.sockets.add(sock);
        let start = rt::sys_uptime_ms();
        loop {
            self.poll();
            let s = self.sockets.get::<tcp::Socket>(handle);
            if s.may_send() {
                return Some(handle);
            }
            if !s.is_active() {
                self.sockets.remove(handle);
                return None; // refused (RST) or closed before establishing
            }
            if rt::sys_uptime_ms() - start > 8000 {
                self.sockets.remove(handle);
                return None; // timed out
            }
        }
    }

    /// Enqueue as much of `data` as the TCP send buffer accepts and pump until it has
    /// left the buffer. Returns Some(bytes_accepted) — which may be < data.len() if the
    /// send buffer was nearly full, so the caller must loop — or None if the socket can no
    /// longer send (closed/reset).
    pub fn send(&mut self, handle: SocketHandle, data: &[u8]) -> Option<usize> {
        let queued = {
            let s = self.sockets.get_mut::<tcp::Socket>(handle);
            if !s.may_send() {
                return None;
            }
            match s.send_slice(data) {
                Ok(0) => return Some(0), // buffer full right now; caller retries
                Ok(n) => n,
                Err(_) => return None,
            }
        };
        let start = rt::sys_uptime_ms();
        loop {
            self.poll();
            let s = self.sockets.get::<tcp::Socket>(handle);
            if s.send_queue() == 0 {
                return Some(queued);
            }
            if rt::sys_uptime_ms() - start > 8000 {
                return Some(queued); // pumped what we could; bytes are acked or will be
            }
        }
    }

    /// Pump until data is available; copy up to `out.len()` bytes. Returns 0 when
    /// the peer has closed with nothing more to read, or on timeout.
    pub fn recv(&mut self, handle: SocketHandle, out: &mut [u8]) -> usize {
        let start = rt::sys_uptime_ms();
        loop {
            self.poll();
            let s = self.sockets.get_mut::<tcp::Socket>(handle);
            if s.can_recv() {
                return s.recv_slice(out).unwrap_or(0);
            }
            if !s.may_recv() {
                return 0; // peer closed, no buffered data
            }
            if rt::sys_uptime_ms() - start > 8000 {
                return 0;
            }
        }
    }

    /// Non-blocking receive: poll once, then return `(bytes, would_block)`. `would_block`
    /// = true means the socket is open but has no data buffered yet — the caller should
    /// yield and retry rather than hog this single-threaded server. `(0, false)` = the
    /// peer closed. This is what lets two LOCAL clients (e.g. an X client and Xwayland over
    /// loopback) make progress: a blocking recv would pin the net server for 8s, starving
    /// the very peer whose send would produce the awaited data.
    pub fn recv_nb(&mut self, handle: SocketHandle, out: &mut [u8]) -> (usize, bool) {
        self.poll();
        let s = self.sockets.get_mut::<tcp::Socket>(handle);
        if s.can_recv() {
            (s.recv_slice(out).unwrap_or(0), false)
        } else if !s.may_recv() {
            (0, false) // peer closed, nothing buffered
        } else {
            (0, true) // open but empty — would block
        }
    }

    /// Close the connection (send FIN, pump briefly) and drop the socket.
    pub fn close(&mut self, handle: SocketHandle) {
        self.sockets.get_mut::<tcp::Socket>(handle).close();
        for _ in 0..64 {
            self.poll();
        }
        self.sockets.remove(handle);
    }
}
