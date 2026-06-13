//! Minimal DNS (RFC 1035): build an A-record query, parse the first A answer.
use alloc::vec::Vec;

/// Build a standard recursive A-record query for `name` (e.g. "example.com").
pub fn query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::new();
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: recursion desired
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    q.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    q.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    q.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0); // root label terminates QNAME
    q.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    q
}

/// Skip a (possibly compressed) DNS name, returning the offset just past it.
fn skip_name(p: &[u8], mut off: usize) -> Option<usize> {
    loop {
        let b = *p.get(off)?;
        if b & 0xC0 == 0xC0 {
            return Some(off + 2); // compression pointer ends the name
        }
        if b == 0 {
            return Some(off + 1);
        }
        off += 1 + b as usize;
    }
}

/// Parse the first A (IPv4) answer out of a DNS response.
pub fn first_a(resp: &[u8]) -> Option<[u8; 4]> {
    if resp.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([resp[4], resp[5]]);
    let an = u16::from_be_bytes([resp[6], resp[7]]);
    let mut off = 12;
    for _ in 0..qd {
        off = skip_name(resp, off)?;
        off += 4; // QTYPE + QCLASS
    }
    for _ in 0..an {
        off = skip_name(resp, off)?;
        if off + 10 > resp.len() {
            return None;
        }
        let typ = u16::from_be_bytes([resp[off], resp[off + 1]]);
        let rdlen = u16::from_be_bytes([resp[off + 8], resp[off + 9]]) as usize;
        off += 10;
        if typ == 1 && rdlen == 4 && off + 4 <= resp.len() {
            return Some([resp[off], resp[off + 1], resp[off + 2], resp[off + 3]]);
        }
        off += rdlen;
    }
    None
}
