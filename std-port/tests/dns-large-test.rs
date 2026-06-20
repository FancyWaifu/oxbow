#![no_main]
extern crate oxbow_rt;
use std::net::ToSocketAddrs;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // Mix of small (example.com ~45B) and large (multi-A / CNAME-chain) responses to
    // exercise the shared-frame path beyond the old 56-byte inline cap.
    for host in ["example.com", "google.com", "www.microsoft.com"] {
        match (host, 80u16).to_socket_addrs() {
            Ok(a) => {
                let v: Vec<_> = a.collect();
                println!("DNS: {host} -> {v:?}");
            }
            Err(e) => println!("DNS: {host} ERR {e}"),
        }
    }
    println!("DNS: done");
    std::process::exit(0);
}
