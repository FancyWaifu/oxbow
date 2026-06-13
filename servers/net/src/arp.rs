//! ARP (RFC 826) — resolve an IPv4 address to a MAC, with a small cache.
use alloc::vec::Vec;

pub const OP_REQUEST: u16 = 1;
pub const OP_REPLY: u16 = 2;
const HTYPE_ETH: u16 = 1;
const PTYPE_IPV4: u16 = 0x0800;

/// Build a 28-byte ARP packet (the payload of an Ethernet ARP frame).
pub fn packet(op: u16, sha: [u8; 6], spa: [u8; 4], tha: [u8; 6], tpa: [u8; 4]) -> Vec<u8> {
    let mut p = Vec::with_capacity(28);
    p.extend_from_slice(&HTYPE_ETH.to_be_bytes());
    p.extend_from_slice(&PTYPE_IPV4.to_be_bytes());
    p.push(6); // hlen
    p.push(4); // plen
    p.extend_from_slice(&op.to_be_bytes());
    p.extend_from_slice(&sha);
    p.extend_from_slice(&spa);
    p.extend_from_slice(&tha);
    p.extend_from_slice(&tpa);
    p
}

pub struct Arp {
    pub op: u16,
    pub sha: [u8; 6],
    pub spa: [u8; 4],
    pub tpa: [u8; 4],
}

pub fn parse(p: &[u8]) -> Option<Arp> {
    if p.len() < 28 {
        return None;
    }
    if u16::from_be_bytes([p[0], p[1]]) != HTYPE_ETH
        || u16::from_be_bytes([p[2], p[3]]) != PTYPE_IPV4
    {
        return None;
    }
    let op = u16::from_be_bytes([p[6], p[7]]);
    let mut sha = [0u8; 6];
    sha.copy_from_slice(&p[8..14]);
    let mut spa = [0u8; 4];
    spa.copy_from_slice(&p[14..18]);
    let mut tpa = [0u8; 4];
    tpa.copy_from_slice(&p[24..28]);
    Some(Arp { op, sha, spa, tpa })
}

/// A tiny direct-mapped ARP cache (IPv4 -> MAC).
pub struct Cache {
    entries: [([u8; 4], [u8; 6], bool); 8],
    next: usize,
}

impl Cache {
    pub const fn new() -> Self {
        Cache { entries: [([0; 4], [0; 6], false); 8], next: 0 }
    }

    pub fn insert(&mut self, ip: [u8; 4], mac: [u8; 6]) {
        for e in self.entries.iter_mut() {
            if e.2 && e.0 == ip {
                e.1 = mac;
                return;
            }
        }
        self.entries[self.next] = (ip, mac, true);
        self.next = (self.next + 1) % self.entries.len();
    }

    pub fn lookup(&self, ip: [u8; 4]) -> Option<[u8; 6]> {
        self.entries.iter().find(|e| e.2 && e.0 == ip).map(|e| e.1)
    }
}
