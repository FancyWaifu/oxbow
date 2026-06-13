//! Minimal DHCP client (RFC 2131): build DISCOVER/REQUEST, parse OFFER/ACK.
//!
//! DHCP rides UDP (client port 68, server port 67) and is broadcast — we have no
//! IP yet, so the request goes to 255.255.255.255 from 0.0.0.0 with the BOOTP
//! broadcast flag set, asking the server to broadcast its reply back.
use alloc::vec;
use alloc::vec::Vec;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const ACK: u8 = 5;

const MAGIC: u32 = 0x6382_5363; // DHCP options magic cookie

/// Build a DHCP message: the 236-byte BOOTP header + magic cookie + options.
/// `req_ip`/`server_id` are emitted as options when present (for REQUEST).
pub fn message(
    xid: u32,
    mac: &[u8; 6],
    msg_type: u8,
    req_ip: Option<[u8; 4]>,
    server_id: Option<[u8; 4]>,
) -> Vec<u8> {
    let mut p = vec![0u8; 240]; // 236 BOOTP fixed + 4 magic cookie
    p[0] = 1; // op = BOOTREQUEST
    p[1] = 1; // htype = Ethernet
    p[2] = 6; // hlen
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // flags: broadcast reply
    p[28..34].copy_from_slice(mac); // chaddr (client hardware address)
    p[236..240].copy_from_slice(&MAGIC.to_be_bytes());
    // Options.
    p.push(53);
    p.push(1);
    p.push(msg_type); // DHCP message type
    if let Some(ip) = req_ip {
        p.push(50);
        p.push(4);
        p.extend_from_slice(&ip); // requested IP address
    }
    if let Some(sid) = server_id {
        p.push(54);
        p.push(4);
        p.extend_from_slice(&sid); // server identifier
    }
    p.extend_from_slice(&[55, 3, 1, 3, 6]); // param request: mask, router, DNS
    p.push(255); // end
    p
}

pub struct Reply {
    pub xid: u32,
    pub msg_type: u8,
    pub yiaddr: [u8; 4],
    pub server_id: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
    pub mask: [u8; 4],
}

/// Parse a DHCP reply (the UDP payload of a server message).
pub fn parse(p: &[u8]) -> Option<Reply> {
    if p.len() < 240 || u32::from_be_bytes([p[236], p[237], p[238], p[239]]) != MAGIC {
        return None;
    }
    let xid = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    let mut r = Reply {
        xid,
        msg_type: 0,
        yiaddr: [p[16], p[17], p[18], p[19]],
        server_id: [0; 4],
        router: [0; 4],
        dns: [0; 4],
        mask: [0; 4],
    };
    let mut i = 240;
    while i < p.len() {
        let opt = p[i];
        if opt == 255 {
            break;
        }
        if opt == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= p.len() {
            break;
        }
        let len = p[i + 1] as usize;
        let start = i + 2;
        if start + len > p.len() {
            break;
        }
        let d = &p[start..start + len];
        match opt {
            53 if len >= 1 => r.msg_type = d[0],
            54 if len >= 4 => r.server_id.copy_from_slice(&d[..4]),
            1 if len >= 4 => r.mask.copy_from_slice(&d[..4]),
            3 if len >= 4 => r.router.copy_from_slice(&d[..4]),
            6 if len >= 4 => r.dns.copy_from_slice(&d[..4]),
            _ => {}
        }
        i = start + len;
    }
    Some(r)
}
