// UDP MTU echo test: a real Rust std program that drives std::net::UdpSocket
// send_to / recv_from through oxbow-rt's hosted shims, which now ride a per-socket
// zero-copy transfer frame (netmap Stage 2). We echo datagrams of several sizes off
// a host UDP echo server (reachable at 10.0.2.2:ECHO_PORT under QEMU slirp) and
// verify each comes back byte-for-byte. The 400-byte case exercises the inline path;
// 800 and 1400 exercise the >480 frame path that used to truncate/reject.
#![no_main]
extern crate oxbow_rt;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

const ECHO_PORT: u16 = 17171;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8).collect()
}

fn echo_once(sock: &UdpSocket, dst: &SocketAddr, len: usize) -> Result<(), String> {
    let tx = pattern(len);
    let n = sock.send_to(&tx, dst).map_err(|e| format!("send_to({len}) {e}"))?;
    if n != len {
        return Err(format!("send_to({len}) short send: {n}"));
    }
    let mut buf = vec![0u8; 2048];
    let (rn, src) = sock.recv_from(&mut buf).map_err(|e| format!("recv_from({len}) {e}"))?;
    if rn != len {
        return Err(format!("len {len}: echoed {rn} bytes (expected {len})"));
    }
    if buf[..rn] != tx[..] {
        let mismatch = (0..rn).find(|&i| buf[i] != tx[i]).unwrap_or(rn);
        return Err(format!("len {len}: byte mismatch at {mismatch}"));
    }
    println!("UDPMTU: len={len} ok (echoed {rn} from {src})");
    Ok(())
}

fn run() -> Result<(), String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind {e}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("set_read_timeout {e}"))?;
    let dst = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), ECHO_PORT));
    for len in [400usize, 800, 1400] {
        echo_once(&sock, &dst, len)?;
    }
    Ok(())
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    match run() {
        Ok(()) => println!("UDPMTU: ALL OK"),
        Err(e) => println!("UDPMTU: FAIL {e}"),
    }
    std::process::exit(0);
}
