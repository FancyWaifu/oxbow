#![no_main]
extern crate oxbow_rt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // 1. Resolve a few real hostnames via std DNS.
    for host in ["example.com", "one.one.one.one"] {
        match (host, 80u16).to_socket_addrs() {
            Ok(a) => {
                let v: Vec<_> = a.collect();
                println!("DNS: {host} -> {v:?}");
            }
            Err(e) => println!("DNS: {host} ERR {e}"),
        }
    }
    // 2. End-to-end: connect by NAME (resolve + TCP via the net server) and HTTP HEAD.
    match TcpStream::connect("example.com:80") {
        Ok(mut s) => {
            println!("DNS: connected to example.com:80 (peer {:?})", s.peer_addr());
            let _ = s.write_all(b"HEAD / HTTP/1.0\r\nHost: example.com\r\n\r\n");
            let mut buf = [0u8; 80];
            let n = s.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n]);
            println!("DNS: HTTP <- {}", line.lines().next().unwrap_or("(nothing)"));
        }
        Err(e) => println!("DNS: connect ERR {e}"),
    }
    println!("DNS: done");
    std::process::exit(0);
}
