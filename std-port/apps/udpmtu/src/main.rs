// UDP MTU echo test: a real Rust std program that drives std::net::UdpSocket
// send_to / recv_from through oxbow-rt's hosted shims, which now ride a per-socket
// zero-copy transfer frame (netmap Stage 2). We echo datagrams of several sizes off
// a host UDP echo server (reachable at 10.0.2.2:ECHO_PORT under QEMU slirp) and
// verify each comes back byte-for-byte. The 400-byte case exercises the inline path;
// 800 and 1400 exercise the >480 frame path that used to truncate/reject.
#![no_main]
extern crate oxbow_rt;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::Duration;

const ECHO_PORT: u16 = 17171;
const TCP_ECHO_PORT: u16 = 17172;

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

// TCP: connect to the host echo, write `len` bytes (write_all loops over MTU-sized
// frame chunks), read them all back, verify byte-for-byte. len > 504 exercises the
// per-socket frame send AND recv paths; multi-MTU len spans several frame chunks.
fn tcp_echo(len: usize) -> Result<(), String> {
    let dst = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), TCP_ECHO_PORT));
    let mut s = TcpStream::connect(dst).map_err(|e| format!("tcp connect {e}"))?;
    // Best-effort: wire TCP streams don't support read timeouts on oxbow; the server
    // recv blocks until data/close and the echo replies promptly, so we don't need one.
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let tx = pattern(len);
    s.write_all(&tx).map_err(|e| format!("tcp write_all({len}) {e}"))?;
    let mut rx = vec![0u8; len];
    s.read_exact(&mut rx).map_err(|e| format!("tcp read_exact({len}) {e}"))?;
    if rx != tx {
        let mismatch = (0..len).find(|&i| rx[i] != tx[i]).unwrap_or(len);
        return Err(format!("tcp len {len}: byte mismatch at {mismatch}"));
    }
    println!("UDPMTU: tcp len={len} ok (echoed {len})");
    Ok(())
}

// Stage 3: the batched ring + doorbell. Queue 8 datagrams into the TX ring (pure
// memory writes), then kick ONCE — the server sends all 8 in a single domain
// crossing. Harvest the 8 echoes back through the RX ring over follow-up kicks, and
// verify each round-trips. Proves "one crossing per batch": 8 sends, 1 TX kick.
fn ring_batch() -> Result<(), String> {
    use oxbow_abi::BOOT_NET_EP;
    use oxbow_rt::{ring, udp};

    // A ring of RING_SLOTS holds RING_SLOTS-1 entries (the classic head==tail-is-empty
    // discipline sacrifices one slot), so 7 fit before a drain.
    const N: usize = 7;
    const PLEN: usize = 200;
    let dst = [10, 0, 2, 2];

    let (sock, _port) = udp::bind(BOOT_NET_EP, 0).ok_or("ring: bind failed")?;
    let r = match ring::attach(sock) {
        Some(r) => r,
        None => {
            udp::close(sock);
            return Err("ring: attach failed".into());
        }
    };
    // Queue N distinct datagrams: payload[0] = marker, rest = pattern.
    for i in 0..N {
        let mut p = pattern(PLEN);
        p[0] = i as u8;
        if !r.push(dst, ECHO_PORT, &p) {
            udp::close(sock);
            return Err(format!("ring: push {i} (ring full?)"));
        }
    }
    // One doorbell sends the whole batch.
    let (sent, _) = r.kick();
    if sent != N {
        udp::close(sock);
        return Err(format!("ring: kick sent {sent} (expected {N})"));
    }
    // Harvest the echoes over follow-up kicks (they arrive over the wire).
    let mut seen = [false; N];
    let mut got = 0;
    let mut buf = [0u8; 256];
    for _ in 0..200 {
        let _ = r.kick(); // doorbell: harvest whatever has arrived
        while let Some((_src, _port, len)) = r.pop(&mut buf) {
            if len != PLEN {
                udp::close(sock);
                return Err(format!("ring: echo len {len} (expected {PLEN})"));
            }
            let marker = buf[0] as usize;
            let mut p = pattern(PLEN);
            p[0] = marker as u8;
            if marker >= N || seen[marker] || buf[..len] != p[..] {
                udp::close(sock);
                return Err(format!("ring: bad/dup echo marker {marker}"));
            }
            seen[marker] = true;
            got += 1;
        }
        if got == N {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    udp::close(sock);
    if got != N {
        return Err(format!("ring: harvested {got}/{N} echoes"));
    }
    println!("UDPMTU: ring ok ({N} datagrams, 1 TX kick, {got} echoes harvested)");
    Ok(())
}

// Stage 3 async RX: register a notification, send a TX batch, then BLOCK on the notif
// (no poll-kick) until the server's IRQ-driven pump fills the RX ring and signals us.
// Proves the kernel bound-notification (sys_recv_notif) end-to-end: the net server is
// idle in a multiplexed wait and the NIC IRQ wakes it to deliver our datagrams.
fn ring_async() -> Result<(), String> {
    use oxbow_abi::BOOT_NET_EP;
    use oxbow_rt::{ring, sys_notif_create, sys_notif_wait, udp};

    const N: usize = 7;
    const PLEN: usize = 200;
    let dst = [10, 0, 2, 2];

    let (sock, _port) = udp::bind(BOOT_NET_EP, 0).ok_or("async: bind failed")?;
    let r = match ring::attach(sock) {
        Some(r) => r,
        None => {
            udp::close(sock);
            return Err("async: ring attach failed".into());
        }
    };
    let notif = match sys_notif_create() {
        Ok(n) => n,
        Err(_) => {
            udp::close(sock);
            return Err("async: notif_create failed".into());
        }
    };
    if !r.set_rxnotif(notif) {
        udp::close(sock);
        return Err("async: set_rxnotif failed".into());
    }
    for i in 0..N {
        let mut p = pattern(PLEN);
        p[0] = i as u8;
        if !r.push(dst, ECHO_PORT, &p) {
            udp::close(sock);
            return Err(format!("async: push {i}"));
        }
    }
    let (sent, _) = r.kick(); // TX only — RX arrives via the pump + notif
    if sent != N {
        udp::close(sock);
        return Err(format!("async: kick sent {sent}"));
    }
    let mut seen = [false; N];
    let mut got = 0;
    let mut buf = [0u8; 256];
    let mut waits = 0;
    while got < N && waits < 64 {
        // BLOCK until the server's pump signals RX (async — no busy poll-kick).
        let _ = sys_notif_wait(notif);
        waits += 1;
        while let Some((_src, _port, len)) = r.pop(&mut buf) {
            if len != PLEN {
                udp::close(sock);
                return Err(format!("async: echo len {len}"));
            }
            let marker = buf[0] as usize;
            let mut p = pattern(PLEN);
            p[0] = marker as u8;
            if marker >= N || seen[marker] || buf[..len] != p[..] {
                udp::close(sock);
                return Err(format!("async: bad/dup marker {marker}"));
            }
            seen[marker] = true;
            got += 1;
        }
    }
    udp::close(sock);
    if got != N {
        return Err(format!("async: harvested {got}/{N} (waits {waits})"));
    }
    println!("UDPMTU: async ok ({N} datagrams via notif wait, {waits} wakeups)");
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
    // TCP frame path: 200 B (inline ≤504), 1400 B (one frame chunk), 4000 B (multi-MTU).
    for len in [200usize, 1400, 4000] {
        tcp_echo(len)?;
    }
    // Stage 3: batched ring + doorbell.
    ring_batch()?;
    // Stage 3 async RX: block on a notif, woken by the server's IRQ pump.
    ring_async()?;
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
