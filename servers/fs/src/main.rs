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
//! Storage: file bytes live in an ARENA the server `sys_map`s from its own Memory
//! budget (law L6 — even the filesystem funds its storage from a capability).
//! The tree is seeded from a USTAR tar initrd mapped read-only at FS_INITRD;
//! seed files are copied into the arena so every file is uniformly writable.
#![no_std]
#![no_main]

use oxbow_abi::{
    MsgBuf, BOOT_CONSOLE, BOOT_EP, BOOT_MEM, FS_DIR, FS_FILE, FS_INITRD, PROT_READ, PROT_WRITE,
    R_GRANT, R_SEND, TAG_FS_CREATE, TAG_FS_MKDIR, TAG_FS_OPEN, TAG_FS_READ, TAG_FS_READDIR,
    TAG_FS_RENAME, TAG_FS_UNLINK, TAG_FS_WRITE,
};
use oxbow_rt as rt;

const MAX_NODES: usize = 256;
const NAME_MAX: usize = 40;
const READ_CHUNK: usize = 56; // 7 u64 of data[1..8]

/// Mutable file storage, mapped from the fs Memory budget. The arena hands out
/// fixed `BLOCK` regions; a file is a list of up to `MAX_BLOCKS` blocks, so it
/// can GROW past one block (up to `MAX_BLOCKS * BLOCK`). Sized for real output:
/// an 8 MiB arena of page-sized blocks, up to 512 KiB per file — enough to hold
/// a compiled binary, the prerequisite for tcc emitting an output file (arc 2 of
/// self-hosting). The fs server is granted a matching budget at boot (§main).
/// (MAX_BLOCKS is bounded by the fs's 512 KiB stack, where the `nodes` array
/// lives: 256 nodes * (MAX_BLOCKS*8 + header) must stay well under it. Lifting
/// the ceiling further means moving `nodes` to static storage.)
const ARENA: usize = 0x2000_0000;
const ARENA_SIZE: usize = 8 * 1024 * 1024;
const BLOCK: usize = 4096;
const MAX_BLOCKS: usize = 128; // 512 KiB max file
const TOTAL_BLOCKS: usize = ARENA_SIZE / BLOCK;

#[derive(Clone, Copy)]
struct Node {
    kind: u64, // 0 = free, FS_DIR, FS_FILE
    name: [u8; NAME_MAX],
    name_len: usize,
    parent: u16,
    // A file is EITHER read-only and initrd-backed (`data` = absolute address of
    // its bytes in the mapped tar, no arena/size cap) OR writable and arena-backed
    // (`blocks`). `data != 0` marks an initrd file.
    data: usize,
    blocks: [usize; MAX_BLOCKS], // arena offsets of this (writable) file's blocks
    nblocks: usize,
    len: usize, // current size in bytes
}

const FREE: Node = Node {
    kind: 0,
    name: [0; NAME_MAX],
    name_len: 0,
    parent: 0,
    data: 0,
    blocks: [0; MAX_BLOCKS],
    nblocks: 0,
    len: 0,
};

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// A bare node (name + kind), data/arena fields zeroed.
fn mk(name: &[u8], parent: u16, kind: u64) -> Node {
    let mut nb = [0u8; NAME_MAX];
    let n = core::cmp::min(name.len(), NAME_MAX);
    nb[..n].copy_from_slice(&name[..n]);
    Node { kind, name: nb, name_len: n, parent, data: 0, blocks: [0; MAX_BLOCKS], nblocks: 0, len: 0 }
}

/// The file-storage arena: a bump pointer plus a free list of fixed `BLOCK`
/// regions. `alloc` pops a reclaimed block before extending the bump pointer, so
/// repeated create/rm cycles (and shrinking files) don't leak the arena.
struct Arena {
    used: usize,
    free: [usize; TOTAL_BLOCKS],
    free_n: usize,
}

impl Arena {
    const fn new() -> Self {
        Arena { used: 0, free: [0; TOTAL_BLOCKS], free_n: 0 }
    }
    /// Allocate one `BLOCK` region; `None` if the arena is full.
    fn alloc(&mut self) -> Option<usize> {
        if self.free_n > 0 {
            self.free_n -= 1;
            return Some(self.free[self.free_n]);
        }
        if self.used + BLOCK > ARENA_SIZE {
            return None;
        }
        let off = self.used;
        self.used += BLOCK;
        Some(off)
    }
    /// Return a block to the free list.
    fn free(&mut self, off: usize) {
        if self.free_n < self.free.len() {
            self.free[self.free_n] = off;
            self.free_n += 1;
        }
    }
}

/// Free all of a file node's blocks back to the arena (on remove or truncate).
fn free_blocks(node: &mut Node, arena: &mut Arena) {
    for i in 0..node.nblocks {
        arena.free(node.blocks[i]);
    }
    node.nblocks = 0;
    node.len = 0;
}

/// A name is a single path component: reject empties, anything with '/', and '..'
/// (capability confinement — a dir cap can't reach above its own subtree).
fn name_ok(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'/') && name != b".." && name != b"."
}

/// Find a child of `parent` named `name` (single component), or None.
fn find_child(nodes: &[Node], parent: usize, name: &[u8]) -> Option<usize> {
    (1..MAX_NODES).find(|&i| {
        let nd = &nodes[i];
        nd.kind != 0 && nd.parent as usize == parent && &nd.name[..nd.name_len] == name
    })
}

/// Resolve a (possibly multi-component) path from `start`, returning the final
/// node. Descends only: '/' separates components, '.' and '..' are rejected, so a
/// path can never escape above the directory capability it was invoked through
/// (confinement). Empty components (leading/trailing/double slash) are tolerated.
fn walk(nodes: &[Node], start: usize, path: &[u8]) -> Option<usize> {
    let mut node = start;
    for comp in path.split(|&b| b == b'/') {
        if comp.is_empty() {
            continue;
        }
        if !name_ok(comp) || nodes[node].kind != FS_DIR {
            return None;
        }
        node = find_child(nodes, node, comp)?;
    }
    Some(node)
}

/// Resolve all but the LAST component of `path` from `start`, returning
/// (parent directory node, last component). The last component is not looked up
/// (it may not exist yet — for create/mkdir). Parent must exist and be a dir.
fn walk_parent<'a>(nodes: &[Node], start: usize, path: &'a [u8]) -> Option<(usize, &'a [u8])> {
    match path.iter().rposition(|&b| b == b'/') {
        None => Some((start, path)),
        Some(i) => {
            let parent = walk(nodes, start, &path[..i])?;
            if nodes[parent].kind != FS_DIR {
                return None;
            }
            Some((parent, &path[i + 1..]))
        }
    }
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

/// Find a direct child directory named `name` under `parent`, creating it if
/// absent. Returns None only if a non-dir with that name exists or we are full.
fn find_or_make_dir(nodes: &mut [Node], next: &mut usize, parent: usize, name: &[u8]) -> Option<usize> {
    if let Some(i) = find_child(nodes, parent, name) {
        return if nodes[i].kind == FS_DIR { Some(i) } else { None };
    }
    if *next >= nodes.len() {
        return None;
    }
    nodes[*next] = mk(name, parent as u16, FS_DIR);
    let id = *next;
    *next += 1;
    Some(id)
}

/// Resolve `path`'s parent directory from the root, creating any intermediate
/// directories. Returns `(parent_dir_id, last_component)`.
fn resolve_parent<'a>(
    nodes: &mut [Node],
    next: &mut usize,
    mut path: &'a [u8],
) -> Option<(usize, &'a [u8])> {
    let mut dir = 1usize; // root
    loop {
        match path.iter().position(|&b| b == b'/') {
            Some(i) => {
                let comp = &path[..i];
                if !comp.is_empty() {
                    dir = find_or_make_dir(nodes, next, dir, comp)?;
                }
                path = &path[i + 1..];
            }
            None => return Some((dir, path)),
        }
    }
}

/// Build the node tree from the USTAR initrd at FS_INITRD. Directories nest
/// (intermediate dirs are created on demand, robust to tar ordering); regular
/// files are referenced IN PLACE — `data` points at their bytes in the mapped
/// tar, so a read-only source file has no 16 KiB cap and uses no arena.
fn build_tree(nodes: &mut [Node]) {
    nodes[1] = mk(b"", 0, FS_DIR);
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
        let trailing_slash = nm.ends_with(b"/");
        if trailing_slash {
            nm = &nm[..nm.len() - 1];
        }
        let size = parse_octal(unsafe { core::slice::from_raw_parts(hdr.add(124), 12) });
        let typeflag = unsafe { *hdr.add(156) };
        let is_dir = typeflag == b'5' || trailing_slash;
        let is_file = !is_dir && (typeflag == b'0' || typeflag == 0);
        if !nm.is_empty() {
            if let Some((par, basec)) = resolve_parent(nodes, &mut next, nm) {
                if !basec.is_empty() && find_child(nodes, par, basec).is_none() && next < nodes.len() {
                    if is_dir {
                        nodes[next] = mk(basec, par as u16, FS_DIR);
                        next += 1;
                    } else if is_file {
                        let mut nd = mk(basec, par as u16, FS_FILE);
                        nd.data = unsafe { hdr.add(512) } as usize; // in-place tar data
                        nd.len = size;
                        nodes[next] = nd;
                        next += 1;
                    }
                }
            }
        }
        off += 512 + ((size + 511) & !511); // header + content padded to 512
    }
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // Map the file-storage arena from our Memory budget (read+write).
    let _ = rt::sys_map(BOOT_MEM, ARENA as u64, ARENA_SIZE as u64, PROT_READ | PROT_WRITE);
    let mut arena = Arena::new();
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
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let nlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let name = &bytes[..nlen];
                let mut r = MsgBuf::new(0);
                // `name` may be a multi-component path (a/b/c); walk it.
                let found = if valid && nodes[node_id].kind == FS_DIR {
                    walk(&nodes, node_id, name)
                } else {
                    None
                };
                match found {
                    Some(child) => match rt::sys_mint(BOOT_EP, child as u64, R_SEND | R_GRANT) {
                        Ok(cap) => {
                            r.data[0] = 0; // ok
                            r.data[1] = nodes[child].kind;
                            r.data[2] = nodes[child].len as u64;
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
                    },
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
                    let nd = &nodes[node_id];
                    let avail = nd.len.saturating_sub(off);
                    let mut count = core::cmp::min(READ_CHUNK, avail);
                    let src: *const u8 = if nd.data != 0 {
                        // Read-only initrd file: contiguous bytes in the mapped tar.
                        (nd.data + off) as *const u8
                    } else {
                        // Writable arena file: a read never crosses a block boundary
                        // (READ_CHUNK < BLOCK); the client loops for the next block.
                        let bi = off / BLOCK;
                        let within = off % BLOCK;
                        count = core::cmp::min(count, BLOCK - within);
                        if bi < nd.nblocks {
                            (ARENA + nd.blocks[bi] + within) as *const u8
                        } else {
                            count = 0;
                            core::ptr::null()
                        }
                    };
                    if count > 0 {
                        let dst = r.data.as_mut_ptr() as *mut u8;
                        unsafe { core::ptr::copy_nonoverlapping(src, dst.add(8), count) };
                        r.data[0] = count as u64;
                    } else {
                        r.data[0] = 0;
                    }
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
            TAG_FS_CREATE => {
                // Create-or-truncate a file under the dir, return a badged cap.
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let nlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let name = &bytes[..nlen];
                let mut r = MsgBuf::new(0);
                // Resolve the parent path; create `base` (the last component) in it.
                let target = if valid && nodes[node_id].kind == FS_DIR {
                    walk_parent(&nodes, node_id, name)
                } else {
                    None
                };
                let child = match target {
                    Some((par, base)) if name_ok(base) => {
                        match find_child(&nodes, par, base) {
                            // A read-only initrd file can't be truncated/overwritten.
                            Some(i) if nodes[i].kind == FS_FILE && nodes[i].data != 0 => None,
                            Some(i) if nodes[i].kind == FS_FILE => {
                                free_blocks(&mut nodes[i], &mut arena); // truncate
                                Some(i)
                            }
                            Some(_) => None, // exists but is a directory
                            None => match (1..MAX_NODES).find(|&i| nodes[i].kind == 0) {
                                Some(i) => {
                                    // Empty file; WRITE allocates blocks on demand.
                                    nodes[i] = mk(base, par as u16, FS_FILE);
                                    Some(i)
                                }
                                None => None,
                            },
                        }
                    }
                    _ => None,
                };
                match child {
                    Some(i) => match rt::sys_mint(BOOT_EP, i as u64, R_SEND | R_GRANT) {
                        Ok(cap) => {
                            r.data[0] = 0;
                            r.data_len = 1;
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
                    },
                    None => {
                        r.data[0] = 1;
                        r.data_len = 1;
                        let _ = rt::sys_reply(reply, &r);
                    }
                }
            }
            TAG_FS_WRITE => {
                let mut r = MsgBuf::new(0);
                let off = m.data[0] as usize;
                let count = (m.data[1] as usize).min(48);
                let mut written = 0usize;
                // Read-only initrd files (data != 0) reject writes; only writable
                // arena-backed files accept them.
                if valid && nodes[node_id].kind == FS_FILE && nodes[node_id].data == 0 {
                    let src = unsafe { (m.data.as_ptr() as *const u8).add(16) };
                    while written < count {
                        let pos = off + written;
                        let bi = pos / BLOCK;
                        let within = pos % BLOCK;
                        if bi >= MAX_BLOCKS {
                            break; // file-size ceiling reached
                        }
                        // Grow: allocate blocks (zeroing any gap) up to `bi`.
                        while nodes[node_id].nblocks <= bi {
                            match arena.alloc() {
                                Some(b) => {
                                    let nb = nodes[node_id].nblocks;
                                    nodes[node_id].blocks[nb] = b;
                                    nodes[node_id].nblocks += 1;
                                }
                                None => break,
                            }
                        }
                        if nodes[node_id].nblocks <= bi {
                            break; // arena exhausted
                        }
                        let n = (count - written).min(BLOCK - within);
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                src.add(written),
                                (ARENA + nodes[node_id].blocks[bi] + within) as *mut u8,
                                n,
                            );
                        }
                        written += n;
                    }
                    if off + written > nodes[node_id].len {
                        nodes[node_id].len = off + written;
                    }
                }
                r.data[0] = written as u64;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_MKDIR => {
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let nlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let name = &bytes[..nlen];
                let mut r = MsgBuf::new(0);
                let target = if valid && nodes[node_id].kind == FS_DIR {
                    walk_parent(&nodes, node_id, name)
                } else {
                    None
                };
                let ok = match target {
                    Some((par, base))
                        if name_ok(base) && find_child(&nodes, par, base).is_none() =>
                    {
                        match (1..MAX_NODES).find(|&i| nodes[i].kind == 0) {
                            Some(i) => {
                                nodes[i] = mk(base, par as u16, FS_DIR);
                                true
                            }
                            None => false,
                        }
                    }
                    _ => false,
                };
                r.data[0] = if ok { 0 } else { 1 };
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_UNLINK => {
                // Remove a file, or an EMPTY directory, under the dir.
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let nlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let name = &bytes[..nlen];
                let mut r = MsgBuf::new(0);
                let target = if valid && nodes[node_id].kind == FS_DIR {
                    walk_parent(&nodes, node_id, name)
                } else {
                    None
                };
                let status = if let Some((par, base)) = target.filter(|(_, b)| name_ok(b)) {
                    match find_child(&nodes, par, base) {
                        Some(i) => {
                            if nodes[i].kind == FS_DIR {
                                let has_children = (1..MAX_NODES)
                                    .any(|j| nodes[j].kind != 0 && nodes[j].parent as usize == i);
                                if has_children {
                                    2 // directory not empty
                                } else {
                                    nodes[i].kind = 0;
                                    0
                                }
                            } else {
                                // file: reclaim all its blocks, then free the slot.
                                free_blocks(&mut nodes[i], &mut arena);
                                nodes[i].kind = 0;
                                0
                            }
                        }
                        None => 1, // not found
                    }
                } else {
                    1
                };
                r.data[0] = status;
                r.data_len = 1;
                let _ = rt::sys_reply(reply, &r);
            }
            TAG_FS_RENAME => {
                // data = old name NUL, then new name NUL.
                let bytes = unsafe { core::slice::from_raw_parts(m.data.as_ptr() as *const u8, 64) };
                let oldlen = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                let old = &bytes[..oldlen];
                let nstart = oldlen + 1;
                let new = if nstart < 64 {
                    let rest = &bytes[nstart..];
                    let nl = rest.iter().position(|&b| b == 0).unwrap_or(0);
                    &rest[..nl]
                } else {
                    &bytes[0..0]
                };
                let mut r = MsgBuf::new(0);
                // Resolve src and dst parents independently — supports moving a
                // node across directories (mv a/x b/y), all within the dir cap.
                let src = if valid && nodes[node_id].kind == FS_DIR {
                    walk_parent(&nodes, node_id, old)
                } else {
                    None
                };
                let dst = if valid && nodes[node_id].kind == FS_DIR {
                    walk_parent(&nodes, node_id, new)
                } else {
                    None
                };
                let status = match (src, dst) {
                    (Some((spar, sbase)), Some((dpar, dbase)))
                        if name_ok(sbase)
                            && name_ok(dbase)
                            && find_child(&nodes, dpar, dbase).is_none() =>
                    {
                        match find_child(&nodes, spar, sbase) {
                            Some(i) => {
                                let mut nb = [0u8; NAME_MAX];
                                let k = core::cmp::min(dbase.len(), NAME_MAX);
                                nb[..k].copy_from_slice(&dbase[..k]);
                                nodes[i].name = nb;
                                nodes[i].name_len = k;
                                nodes[i].parent = dpar as u16; // re-parent (cross-dir)
                                0
                            }
                            None => 1,
                        }
                    }
                    _ => 1,
                };
                r.data[0] = status;
                r.data_len = 1;
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
