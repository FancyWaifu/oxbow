//! fs — the userspace in-memory filesystem (ramfs) server.
//!
//! Reached entirely through capabilities: a client holds a BADGED endpoint to
//! this server, where the badge = the node id of a directory or file (§14/§15).
//! Directories are capabilities — you OPEN a name relative to a directory cap you
//! already hold; there is no global namespace (law L3). OPEN mints a fresh badged
//! cap (badge = the resolved child node) and returns it in the reply.
//!
//! The server is STATELESS w.r.t. open files: every request arrives with the
//! kernel-stamped, unforgeable `m.badge` = node id plus a client-supplied offset,
//! so we just index `nodes[badge]`. No open-file table, no fids, no seek state.
//!
//! Phase 2: a hardcoded tree. Phase 4 will seed it from a tar initrd.
#![no_std]
#![no_main]

use oxbow_abi::{
    MsgBuf, BOOT_CONSOLE, BOOT_EP, FS_DIR, FS_FILE, FS_INITRD, R_GRANT, R_SEND, TAG_FS_OPEN,
    TAG_FS_READ, TAG_FS_READDIR,
};
use oxbow_rt as rt;

const MAX_NODES: usize = 16;
const READ_CHUNK: usize = 56; // 7 u64 of data[1..8]

#[derive(Clone, Copy)]
struct Node {
    kind: u64, // 0 = free, FS_DIR, FS_FILE
    name: [u8; 24],
    name_len: usize,
    parent: u16,
    content: &'static [u8],
}

const FREE: Node = Node {
    kind: 0,
    name: [0; 24],
    name_len: 0,
    parent: 0,
    content: &[],
};

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Build a node with `name`, `parent`, `kind`, `content`.
fn mknode(name: &[u8], parent: u16, kind: u64, content: &'static [u8]) -> Node {
    let mut nb = [0u8; 24];
    let n = core::cmp::min(name.len(), 24);
    nb[..n].copy_from_slice(&name[..n]);
    Node { kind, name: nb, name_len: n, parent, content }
}

/// A name is a single path component: reject empties, anything with '/', and '..'
/// (capability confinement — a dir cap can't reach above its own subtree).
fn name_ok(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'/') && name != b".." && name != b"."
}

/// Parse a USTAR octal numeric field (leading spaces, octal digits, then space/NUL).
fn parse_octal(b: &[u8]) -> usize {
    let mut v = 0usize;
    let mut started = false;
    for &c in b {
        if (b'0'..=b'7').contains(&c) {
            v = v * 8 + (c - b'0') as usize;
            started = true;
        } else if started {
            break;
        }
    }
    v
}

/// Build the node tree by parsing the USTAR initrd mapped read-only at FS_INITRD.
/// Top-level regular files become file nodes under the root; content slices point
/// directly into the mapped archive (no copy — read-only ramfs).
fn build_tree(nodes: &mut [Node]) {
    nodes[1] = mknode(b"", 0, FS_DIR, &[]);
    let base = FS_INITRD as *const u8;
    let mut off = 0usize;
    let mut next = 2usize;
    loop {
        let hdr = unsafe { base.add(off) };
        if unsafe { *hdr } == 0 {
            break; // end-of-archive zero block
        }
        let name_raw = unsafe { core::slice::from_raw_parts(hdr, 100) };
        let nlen = name_raw.iter().position(|&b| b == 0).unwrap_or(100);
        let mut nm = &name_raw[..nlen];
        if nm.starts_with(b"./") {
            nm = &nm[2..];
        }
        let size = parse_octal(unsafe { core::slice::from_raw_parts(hdr.add(124), 12) });
        let typeflag = unsafe { *hdr.add(156) };
        let is_file = typeflag == b'0' || typeflag == 0;
        if is_file && !nm.is_empty() && !nm.contains(&b'/') && next < nodes.len() {
            let content: &'static [u8] = unsafe { core::slice::from_raw_parts(hdr.add(512), size) };
            nodes[next] = mknode(nm, 1, FS_FILE, content);
            next += 1;
        }
        off += 512 + ((size + 511) & !511); // header + content padded to 512
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // Build the tree from the tar initrd the kernel mapped at FS_INITRD.
    let mut nodes = [FREE; MAX_NODES];
    build_tree(&mut nodes);

    w(b"[fs] ready\n");

    loop {
        let mut m = MsgBuf::new(0);
        let reply = match rt::sys_recv(BOOT_EP, &mut m) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let node_id = m.badge as usize; // kernel-stamped, unforgeable
        let valid = node_id < MAX_NODES && nodes[node_id].kind != 0;

        match m.tag {
            TAG_FS_OPEN => {
                // The request data holds the NUL-terminated name to resolve.
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let nlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let name = &bytes[..nlen];
                let mut r = MsgBuf::new(0);
                let found = if valid && nodes[node_id].kind == FS_DIR && name_ok(name) {
                    (1..MAX_NODES).find(|&i| {
                        let nd = &nodes[i];
                        nd.kind != 0 && nd.parent as usize == node_id && &nd.name[..nd.name_len] == name
                    })
                } else {
                    None
                };
                match found {
                    Some(child) => {
                        // Mint a badged cap to the child node, hand it back, then
                        // drop our own copy (per-OPEN handle hygiene).
                        match rt::sys_mint(BOOT_EP, child as u64, R_SEND | R_GRANT) {
                            Ok(cap) => {
                                r.data[0] = 0; // status ok
                                r.data[1] = nodes[child].kind;
                                r.data[2] = nodes[child].content.len() as u64;
                                r.data_len = 3;
                                r.handle_count = 1;
                                r.handles[0] = cap;
                                let _ = rt::sys_reply(reply, &r);
                                let _ = rt::sys_close(cap);
                            }
                            Err(_) => {
                                r.data[0] = 1;
                                r.data_len = 1;
                                let _ = rt::sys_reply(reply, &r);
                            }
                        }
                    }
                    None => {
                        r.data[0] = 1; // not found
                        r.data_len = 1;
                        let _ = rt::sys_reply(reply, &r);
                    }
                }
            }
            TAG_FS_READ => {
                let mut r = MsgBuf::new(0);
                let off = m.data[0] as usize;
                if valid && nodes[node_id].kind == FS_FILE {
                    let content = nodes[node_id].content;
                    let end = core::cmp::min(off + READ_CHUNK, content.len());
                    let count = end.saturating_sub(off);
                    let dst = r.data.as_mut_ptr() as *mut u8;
                    if count > 0 {
                        unsafe {
                            // bytes go into data[1..], i.e. offset 8 in the data array
                            core::ptr::copy_nonoverlapping(
                                content[off..end].as_ptr(),
                                dst.add(8),
                                count,
                            );
                        }
                    }
                    r.data[0] = count as u64;
                    r.data_len = 8;
                } else {
                    r.data[0] = 0; // EOF / bad node
                    r.data_len = 1;
                }
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_READDIR => {
                let cursor = m.data[0] as usize;
                let mut r = MsgBuf::new(0);
                let mut seen = 0usize;
                let mut hit = None;
                if valid && nodes[node_id].kind == FS_DIR {
                    for i in 1..MAX_NODES {
                        let nd = &nodes[i];
                        if nd.kind != 0 && nd.parent as usize == node_id {
                            if seen == cursor {
                                hit = Some(i);
                                break;
                            }
                            seen += 1;
                        }
                    }
                }
                match hit {
                    Some(i) => {
                        r.data[0] = 1; // entry present
                        r.data[1] = nodes[i].kind;
                        let dst = r.data.as_mut_ptr() as *mut u8;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                nodes[i].name.as_ptr(),
                                dst.add(16), // name into data[2..]
                                nodes[i].name_len,
                            );
                        }
                        r.data_len = 8;
                    }
                    None => {
                        r.data[0] = 0; // end of directory
                        r.data_len = 1;
                    }
                }
                let _ = rt::sys_reply(reply, &r);
            }
            _ => {
                let mut r = MsgBuf::new(0);
                r.data[0] = 1;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
        }
    }
}
