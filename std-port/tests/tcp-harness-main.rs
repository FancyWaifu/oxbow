#![no_main]
#![feature(custom_test_frameworks)]
#![feature(core_io_borrowed_buf)]
#![feature(read_buf)]
#![feature(borrowed_buf_init)]
#![feature(io_error_uncategorized)]
#![feature(tcp_linger)]
#![feature(tcp_keepalive)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

// Re-export std modules so the test file's `crate::X` paths resolve.
pub use std::{fmt, io, mem, sync, thread, time};

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
mod tcptests;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
