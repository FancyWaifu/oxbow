// IPv6-on-the-wire demo: TcpStream::connect to a v6 address drives the full path
// (std -> rt __oxbow_tcp_connect6 -> net server connect6 -> smoltcp -> e1000),
// emitting real IPv6 packets (NDP Neighbor Solicitation + TCP SYN over IPv6). The peer
// is unreachable in a no-host-IPv6 environment, so the connect returns refused/timeout,
// but capturing net0 (QEMU filter-dump) shows the guest's IPv6 frames. See cap6.py.
#![no_main]
extern crate oxbow_rt;
use std::net::TcpStream;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let addr = "[2606:4700:4700::1111]:80";
    println!("WIRE6: connecting to {addr}");
    match TcpStream::connect(addr) {
        Ok(_) => println!("WIRE6: connected!"),
        Err(e) => println!("WIRE6: connect result: {e}"),
    }
    println!("WIRE6: done");
    std::process::exit(0);
}
