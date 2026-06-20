#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

// Re-export std modules so the test file's `crate::X` paths resolve.
pub use std::{io, sync, thread, time};

// `crate::net` globs std::net and adds the std test helpers.
pub mod net {
    pub use std::net::*;

    pub mod test {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static PORT: AtomicUsize = AtomicUsize::new(0);
        const BASE_PORT: u16 = 19600;

        pub fn next_test_ip4() -> SocketAddr {
            let port = PORT.fetch_add(1, Ordering::Relaxed) as u16 + BASE_PORT;
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port))
        }
        pub fn next_test_ip6() -> SocketAddr {
            let port = PORT.fetch_add(1, Ordering::Relaxed) as u16 + BASE_PORT;
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), port, 0, 0))
        }
        pub fn sa4(a: Ipv4Addr, p: u16) -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(a, p))
        }
        pub fn sa6(a: Ipv6Addr, p: u16) -> SocketAddr {
            SocketAddr::V6(SocketAddrV6::new(a, p, 0, 0))
        }
        pub fn compare_ignore_zoneid(a: &SocketAddr, b: &SocketAddr) -> bool {
            match (a, b) {
                (SocketAddr::V6(a), SocketAddr::V6(b)) => {
                    a.ip().segments() == b.ip().segments()
                        && a.flowinfo() == b.flowinfo()
                        && a.port() == b.port()
                }
                _ => a == b,
            }
        }
    }
}

#[cfg(test)]
mod udptests;

// Smoke-test the EXTERNAL UDP path through the net server: a DNS query to slirp's
// resolver (10.0.2.3:53) and the reply, verifying send_to + recv_from + the sender
// address now come back from the wire (not the in-process loopback).
#[cfg(test)]
mod external {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
    use std::time::Duration;

    fn dns_query() -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in ["example", "com"] {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&[0, 1, 0, 1]); // QTYPE=A, QCLASS=IN
        q
    }

    #[test]
    fn external_udp_dns() {
        let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        let dns = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 3), 53));
        sock.send_to(&dns_query(), &dns).unwrap();
        let mut buf = [0u8; 512];
        let (n, src) = sock.recv_from(&mut buf).unwrap();
        assert!(n >= 12, "short DNS reply: {n} bytes");
        assert_eq!(src, dns, "reply came from the wrong source");
        assert_eq!(buf[0], 0x12);
        assert_eq!(buf[1], 0x34); // transaction id echoed
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
