#![no_main]
extern crate oxbow_rt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let args: Vec<String> = std::env::args().collect();
    println!("V6PEER: args={args:?}");
    if args.iter().any(|a| a == "listen") {
        let l = TcpListener::bind("[::]:9090").expect("bind");
        println!("V6PEER: listening {:?}", l.local_addr());
        match l.accept() {
            Ok((mut s, peer)) => {
                println!("V6PEER: accepted from {peer}");
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap_or(0);
                println!("V6PEER: got {}", String::from_utf8_lossy(&buf[..n]).trim_end());
                let _ = s.write_all(b"PONG6\n");
                let _ = s.flush();
                println!("V6PEER: replied PONG6");
            }
            Err(e) => println!("V6PEER: accept err {e}"),
        }
    } else if args.iter().any(|a| a == "connect") {
        let addr = "[fec0::a]:9090";
        println!("V6PEER: connecting {addr}");
        match TcpStream::connect(addr) {
            Ok(mut s) => {
                println!("V6PEER: connected to {:?}", s.peer_addr());
                let _ = s.write_all(b"PING6\n");
                let _ = s.flush();
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).unwrap_or(0);
                println!("V6PEER: got {}", String::from_utf8_lossy(&buf[..n]).trim_end());
            }
            Err(e) => println!("V6PEER: connect err {e}"),
        }
    }
    println!("V6PEER: done");
    std::process::exit(0);
}
