//! IPv4 (layer 3) + the internet checksum (RFC 791 / RFC 1071).
use alloc::vec::Vec;

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_UDP: u8 = 17;
pub const PROTO_TCP: u8 = 6;

/// The 16-bit one's-complement internet checksum over `data`.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8; // odd trailing byte is the high half
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Build an IPv4 packet wrapping `payload`, computing the header checksum.
pub fn packet(src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut h = Vec::with_capacity(total);
    h.push(0x45); // version 4, IHL 5 (no options)
    h.push(0x00); // DSCP / ECN
    h.extend_from_slice(&(total as u16).to_be_bytes());
    h.extend_from_slice(&0u16.to_be_bytes()); // identification
    h.extend_from_slice(&0x4000u16.to_be_bytes()); // flags = Don't Fragment
    h.push(64); // TTL
    h.push(proto);
    h.extend_from_slice(&0u16.to_be_bytes()); // checksum (filled below)
    h.extend_from_slice(&src);
    h.extend_from_slice(&dst);
    let csum = checksum(&h);
    h[10..12].copy_from_slice(&csum.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

/// A parsed IPv4 header.
pub struct Ipv4 {
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub proto: u8,
    pub payload_off: usize, // offset of the L4 payload within the IPv4 packet
}

pub fn parse(p: &[u8]) -> Option<Ipv4> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] & 0x0F) as usize * 4;
    if ihl < 20 || p.len() < ihl {
        return None;
    }
    let mut src = [0u8; 4];
    src.copy_from_slice(&p[12..16]);
    let mut dst = [0u8; 4];
    dst.copy_from_slice(&p[16..20]);
    Some(Ipv4 { src, dst, proto: p[9], payload_off: ihl })
}
