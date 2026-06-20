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
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

fn now() -> Instant {
    Instant::from_millis(rt::sys_uptime_ms() as i64)
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
        // No allocation on the empty-ring path (the common case while polling).
        nic.recv_nonblocking(&mut buf).map(|n| (PhyRx { buf, len: n }, PhyTx { nic: self.nic }))
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
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), 24));
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(gw[0], gw[1], gw[2], gw[3]));
        TcpStack {
            device,
            iface,
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
    pub fn accept(&mut self, port: u16) -> Option<(SocketHandle, [u8; 4], u16)> {
        self.poll();
        let idx = self.listeners.iter().position(|&(p, h)| {
            p == port && self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established
        })?;
        let (p, h) = self.listeners.remove(idx);
        let (ip, peer_port) = match self.sockets.get::<tcp::Socket>(h).remote_endpoint() {
            Some(ep) => {
                let ip = match ep.addr {
                    IpAddress::Ipv4(v4) => v4.octets(),
                    #[allow(unreachable_patterns)]
                    _ => [0; 4],
                };
                (ip, ep.port)
            }
            None => ([0; 4], 0),
        };
        if let Some(nh) = self.new_listening_socket(p) {
            self.listeners.push((p, nh));
        }
        Some((h, ip, peer_port))
    }

    fn poll(&mut self) {
        let _ = self.iface.poll(now(), &mut self.device, &mut self.sockets);
    }

    /// Open a connection to `dst:port`. Returns the socket handle once the
    /// three-way handshake completes, or None on refusal/timeout.
    pub fn connect(&mut self, dst: [u8; 4], port: u16) -> Option<SocketHandle> {
        let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
        let mut sock = tcp::Socket::new(rx, tx);
        let local = self.next_port;
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        let remote = IpEndpoint::new(IpAddress::v4(dst[0], dst[1], dst[2], dst[3]), port);
        if sock.connect(self.iface.context(), remote, local).is_err() {
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

    /// Queue `data` and pump until it has all left the send buffer.
    pub fn send(&mut self, handle: SocketHandle, data: &[u8]) -> bool {
        {
            let s = self.sockets.get_mut::<tcp::Socket>(handle);
            if !s.may_send() || s.send_slice(data).is_err() {
                return false;
            }
        }
        let start = rt::sys_uptime_ms();
        loop {
            self.poll();
            let s = self.sockets.get::<tcp::Socket>(handle);
            if s.send_queue() == 0 {
                return true;
            }
            if rt::sys_uptime_ms() - start > 8000 {
                return false;
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

    /// Close the connection (send FIN, pump briefly) and drop the socket.
    pub fn close(&mut self, handle: SocketHandle) {
        self.sockets.get_mut::<tcp::Socket>(handle).close();
        for _ in 0..64 {
            self.poll();
        }
        self.sockets.remove(handle);
    }
}
