//! ICMP (RFC 792) — just enough for echo request/reply (ping).
use crate::ipv4;
use alloc::vec::Vec;

pub const ECHO_REPLY: u8 = 0;
pub const ECHO_REQUEST: u8 = 8;

/// Build an ICMP echo message (request or reply) with its checksum.
pub fn echo(typ: u8, id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + data.len());
    m.push(typ);
    m.push(0); // code
    m.extend_from_slice(&0u16.to_be_bytes()); // checksum (filled below)
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(data);
    let c = ipv4::checksum(&m);
    m[2..4].copy_from_slice(&c.to_be_bytes());
    m
}

pub struct Echo {
    pub typ: u8,
    pub id: u16,
    pub seq: u16,
}

pub fn parse(p: &[u8]) -> Option<Echo> {
    if p.len() < 8 {
        return None;
    }
    Some(Echo {
        typ: p[0],
        id: u16::from_be_bytes([p[4], p[5]]),
        seq: u16::from_be_bytes([p[6], p[7]]),
    })
}
