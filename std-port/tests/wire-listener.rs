// Wire TcpListener demo: bind 0.0.0.0:8080 on the net server, accept one external
// connection, echo a PONG. Run under QEMU with `hostfwd=tcp:127.0.0.1:5555-:8080`
// and connect from the host (see wire-listener-harness.py). Plain oxbow_main program:
// build with -Z build-std=std,panic_unwind (no --tests).
#![no_main]
extern crate oxbow_rt;
use std::io::{Read, Write};
use std::net::TcpListener;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    println!("WIRELISTEN: binding 0.0.0.0:8080");
    let listener = match TcpListener::bind("0.0.0.0:8080") {
        Ok(l) => l,
        Err(e) => { println!("WIRELISTEN: bind failed: {e}"); std::process::exit(1); }
    };
    println!("WIRELISTEN: listening on {:?}", listener.local_addr());
    match listener.accept() {
        Ok((mut stream, peer)) => {
            println!("WIRELISTEN: accepted from {peer}");
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap_or(0);
            println!("WIRELISTEN: got {n} bytes: {}", String::from_utf8_lossy(&buf[..n]).trim_end());
            let _ = stream.write_all(b"PONG\n");
            let _ = stream.flush();
            println!("WIRELISTEN: replied PONG, done");
        }
        Err(e) => println!("WIRELISTEN: accept failed: {e}"),
    }
    std::process::exit(0);
}
