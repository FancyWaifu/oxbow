//! Ethernet II framing (layer 2).
use alloc::vec::Vec;

pub const BROADCAST: [u8; 6] = [0xFF; 6];
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV4: u16 = 0x0800;

/// Wrap `payload` in an Ethernet II frame (the NIC pads short frames + appends
/// the FCS, so no padding/CRC here).
pub fn frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + payload.len());
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// `(dst, src, ethertype, payload_offset)` of a received frame.
pub fn parse(f: &[u8]) -> Option<([u8; 6], [u8; 6], u16, usize)> {
    if f.len() < 14 {
        return None;
    }
    let mut dst = [0u8; 6];
    dst.copy_from_slice(&f[0..6]);
    let mut src = [0u8; 6];
    src.copy_from_slice(&f[6..12]);
    let et = u16::from_be_bytes([f[12], f[13]]);
    Some((dst, src, et, 14))
}
