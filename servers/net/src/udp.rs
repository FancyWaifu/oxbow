//! UDP (RFC 768) — header + the IPv4 pseudo-header checksum.
use crate::ipv4;
use alloc::vec::Vec;

/// Build a UDP datagram (header + payload) with a full pseudo-header checksum.
pub fn segment(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut s = Vec::with_capacity(len);
    s.extend_from_slice(&src_port.to_be_bytes());
    s.extend_from_slice(&dst_port.to_be_bytes());
    s.extend_from_slice(&(len as u16).to_be_bytes());
    s.extend_from_slice(&0u16.to_be_bytes()); // checksum (filled below)
    s.extend_from_slice(payload);

    // Checksum spans the IPv4 pseudo-header followed by the whole datagram.
    let mut pseudo = Vec::with_capacity(12 + len);
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(ipv4::PROTO_UDP);
    pseudo.extend_from_slice(&(len as u16).to_be_bytes());
    pseudo.extend_from_slice(&s);
    let mut c = ipv4::checksum(&pseudo);
    if c == 0 {
        c = 0xFFFF; // a 0 checksum means "not computed"; send all-ones instead
    }
    s[6..8].copy_from_slice(&c.to_be_bytes());
    s
}

pub struct Udp {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload_off: usize,
}

pub fn parse(p: &[u8]) -> Option<Udp> {
    if p.len() < 8 {
        return None;
    }
    Some(Udp {
        src_port: u16::from_be_bytes([p[0], p[1]]),
        dst_port: u16::from_be_bytes([p[2], p[3]]),
        payload_off: 8,
    })
}
