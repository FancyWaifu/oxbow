// oxbow-native net socket-options tests: TcpListener IPV6_V6ONLY semantics and
// UdpSocket multicast join/leave membership (join exercises the real net binding).
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, UdpSocket};

#[test]
fn only_v6_is_true_and_settable() {
    let l = TcpListener::bind("[::1]:0").unwrap();
    assert_eq!(l.only_v6().unwrap(), true); // oxbow [::] listeners are IPv6-only
    l.set_only_v6(true).unwrap(); // no-op, already true
    // dual-stack (V6ONLY=false) is not available on oxbow
    let e = l.set_only_v6(false).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    assert_eq!(l.only_v6().unwrap(), true);
}

#[test]
fn multicast_v4_join_leave() {
    let s = UdpSocket::bind("0.0.0.0:0").unwrap();
    let group = Ipv4Addr::new(224, 0, 0, 251);
    let iface = Ipv4Addr::UNSPECIFIED;
    s.join_multicast_v4(&group, &iface).unwrap(); // binds the net socket + records
    // joining the same group twice is an error (already a member)
    assert_eq!(s.join_multicast_v4(&group, &iface).unwrap_err().kind(), ErrorKind::AddrInUse);
    s.leave_multicast_v4(&group, &iface).unwrap();
    // leaving a group we're not in is an error
    assert_eq!(
        s.leave_multicast_v4(&group, &iface).unwrap_err().kind(),
        ErrorKind::AddrNotAvailable
    );
}

#[test]
fn multicast_v6_join_leave() {
    let s = UdpSocket::bind("0.0.0.0:0").unwrap();
    let group = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
    s.join_multicast_v6(&group, 0).unwrap();
    assert_eq!(s.join_multicast_v6(&group, 0).unwrap_err().kind(), ErrorKind::AddrInUse);
    s.leave_multicast_v6(&group, 0).unwrap();
    assert_eq!(s.leave_multicast_v6(&group, 0).unwrap_err().kind(), ErrorKind::AddrNotAvailable);
}

#[test]
fn multicast_independent_groups() {
    let s = UdpSocket::bind("0.0.0.0:0").unwrap();
    let g1 = Ipv4Addr::new(224, 0, 0, 1);
    let g2 = Ipv4Addr::new(224, 0, 0, 2);
    let iface = Ipv4Addr::UNSPECIFIED;
    s.join_multicast_v4(&g1, &iface).unwrap();
    s.join_multicast_v4(&g2, &iface).unwrap(); // distinct group, also Ok
    s.leave_multicast_v4(&g1, &iface).unwrap();
    // g2 is still joined -> re-joining it errors, leaving it succeeds
    assert_eq!(s.join_multicast_v4(&g2, &iface).unwrap_err().kind(), ErrorKind::AddrInUse);
    s.leave_multicast_v4(&g2, &iface).unwrap();
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
