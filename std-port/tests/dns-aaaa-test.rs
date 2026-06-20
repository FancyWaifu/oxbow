#![no_main]
extern crate oxbow_rt;
use std::net::ToSocketAddrs;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // to_socket_addrs now returns BOTH A (IPv4) and AAAA (IPv6) records.
    for host in ["example.com", "google.com", "cloudflare.com"] {
        match (host, 80u16).to_socket_addrs() {
            Ok(a) => {
                let v: Vec<_> = a.collect();
                let v6 = v.iter().filter(|s| s.is_ipv6()).count();
                println!("DNS: {host} -> {v:?}  ({v6} AAAA)");
            }
            Err(e) => println!("DNS: {host} ERR {e}"),
        }
    }
    println!("DNS: done");
    std::process::exit(0);
}
