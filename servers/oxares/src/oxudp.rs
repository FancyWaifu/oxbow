//! extern "C" UDP helpers that c-ares's socket-function callbacks (cares_glue.c)
//! call to reach oxbow's net server. The whole datagram rides the shared
//! transfer frame (large path), so EDNS / multi-record answers fit.
//!
//! IP convention across the FFI boundary: a u32 packed `a<<24 | b<<16 | c<<8 | d`
//! (so `u32::to_be_bytes` yields `[a, b, c, d]`, the wire order).
use oxbow_abi::BOOT_NET_EP;
use oxbow_rt as rt;

/// Attach (once) to the net server's shared UDP frame; returns the buffer
/// pointer, or null on failure.
#[no_mangle]
pub extern "C" fn ox_udp_attach() -> *mut u8 {
    rt::udp::attach(BOOT_NET_EP).unwrap_or(core::ptr::null_mut())
}

/// Bind a fresh UDP socket; returns its capability handle, or -1.
#[no_mangle]
pub extern "C" fn ox_udp_open() -> i64 {
    match rt::udp::bind(BOOT_NET_EP, 0) {
        Some((cap, _)) => cap as i64,
        None => -1,
    }
}

/// Send the first `len` bytes of the shared frame to `ip:port` on `cap`.
/// Returns 0 on success, -1 on failure.
#[no_mangle]
pub extern "C" fn ox_udp_sendv(cap: u64, ip: u32, port: u16, len: usize) -> i32 {
    if rt::udp::sendv(cap as u32, ip.to_be_bytes(), port, len) {
        0
    } else {
        -1
    }
}

/// Non-blocking receive into the shared frame; returns datagram length (0 = none).
#[no_mangle]
pub extern "C" fn ox_udp_recvv(cap: u64) -> i64 {
    rt::udp::recvv(cap as u32) as i64
}

/// Close a UDP socket capability.
#[no_mangle]
pub extern "C" fn ox_udp_close(cap: u64) {
    let _ = rt::sys_close(cap as u32);
}

/// Milliseconds since boot (the driving loop's deadline clock).
#[no_mangle]
pub extern "C" fn ox_uptime_ms() -> u64 {
    rt::sys_uptime_ms()
}

/// The DHCP-leased DNS resolver IP, packed `a<<24 | b<<16 | c<<8 | d`.
#[no_mangle]
pub extern "C" fn ox_dns_ip() -> u32 {
    u32::from_be_bytes(rt::udp::dns_server(BOOT_NET_EP))
}
