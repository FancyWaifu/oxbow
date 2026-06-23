//! jail — the confinement showcase: a deliberately hostile program that tries to
//! escape its capability sandbox, and watches the kernel deny every attempt.
//!
//! It is spawned like any other program, holding ONLY what the shell handed it:
//!   slot 1 (BOOT_EP)   — a capability to ONE directory (the shell opens /tmp and
//!                        grants it), for the L3 confinement test;
//!   slot 2 (STDOUT)    — a send-only tty endpoint (no Console of its own);
//!   slot 3 (BOOT_MEM)  — a small, metered Memory budget;
//!   slot 20 (NET_EP)   — the network endpoint.
//! Everything else in its handle table is empty. It then runs a battery of escape
//! attempts — each uses a real syscall expecting a denial — and prints a verdict
//! table proving laws L1–L6 (docs/abi-v0.md §1) hold. As a finale it pledges away
//! its memory-mapping rights (§37) and then tries to map anyway: the kernel kills
//! it the instant it breaks its own promise — fail-closed, and ALONE, so the
//! shell prompt returns and every server keeps running.
#![no_std]
#![no_main]

use oxbow_abi::{
    Handle, HANDLE_NULL, MsgBuf, SysError, BOOT_MEM, PLEDGE_IPC, PLEDGE_STDIO, PROT_EXEC, PROT_READ,
    PROT_WRITE, R_ATTENUATE, R_RECV, R_SEND, SPAWN_STDOUT, TAG_FS_CREATE, TAG_FS_NAMESPACE,
    TAG_FS_OPEN, TAG_FS_READ, TAG_TTY_WRITE,
};
use oxbow_rt as rt;

/// The directory capability the shell granted, at slot 1 (BOOT_EP). jail tries
/// to escape it; it should be able to act WITHIN it but never above it.
const DIR_CAP: Handle = 1;

/// Write a short (<63 byte) line to stdout — a granted tty R_SEND endpoint (jail
/// holds no Console of its own, by L1).
fn w(s: &[u8]) {
    let mut m = MsgBuf::new(TAG_TTY_WRITE);
    let n = core::cmp::min(s.len(), 63);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(s.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    let _ = rt::sys_send(SPAWN_STDOUT, &m);
}

/// The kernel error name, for the verdict table.
fn errname(e: SysError) -> &'static [u8] {
    match e {
        SysError::BadHandle => b"E_BADHANDLE",
        SysError::BadType => b"E_BADTYPE",
        SysError::Rights => b"E_RIGHTS",
        SysError::Fault => b"E_FAULT",
        SysError::Msg => b"E_MSG",
        SysError::NoMem => b"E_NOMEM",
        SysError::Gone => b"E_GONE",
        _ => b"E_?",
    }
}

/// Report one escape attempt: `desc` then the outcome. `denied` true = the kernel
/// refused (the law held). Returns 1 if the law held (for the tally), else 0.
fn report(desc: &[u8], denied: bool, ename: &[u8]) -> u32 {
    w(b"  ");
    w(desc);
    if denied {
        w(b" -> DENIED ");
        w(ename);
        w(b" [ok]\n");
        1
    } else {
        w(b" -> NOT DENIED [LEAK!]\n");
        0
    }
}

/// Send a one-name fs request (`name` NUL-terminated) on `cap`; return the reply
/// status (0 = ok), or 0xFF if the call itself failed (e.g. no such capability).
/// Closes any cap handed back so we don't leak handles.
fn fs_call(cap: Handle, tag: u64, name: &[u8]) -> u64 {
    let mut m = MsgBuf::new(tag);
    let n = core::cmp::min(name.len(), 60);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = 8;
    if rt::sys_call(cap, &mut m).is_err() {
        return 0xFF;
    }
    if m.handle_count >= 1 {
        let _ = rt::sys_close(m.handles[0]);
    }
    m.data[0]
}

// ---- crafted-ELF fuzzer: the kernel's sys_spawn_bytes validator must REJECT every
// malformed image (never panic, never load it). Each case is a one-field mutation of an
// otherwise-valid ELF64 ET_EXEC; we spawn it and require an error. This is an executable
// regression for the round-1/2 ELF-loader hardening (bounds, W^X, vaddr range, overlap,
// PT_TLS, phdr-table). jail is unpledged here, so PLEDGE_SPAWN is available.
static mut FBUF: [u8; 8192] = [0; 8192];
unsafe fn w16(o: usize, v: u16) {
    FBUF[o] = v as u8;
    FBUF[o + 1] = (v >> 8) as u8;
}
unsafe fn w32(o: usize, v: u32) {
    for i in 0..4 {
        FBUF[o + i] = (v >> (8 * i)) as u8;
    }
}
unsafe fn w64(o: usize, v: u64) {
    for i in 0..8 {
        FBUF[o + i] = (v >> (8 * i)) as u8;
    }
}
/// Lay down a VALID ELF64 ET_EXEC/x86_64 with `nph` program headers; PH0 is a clean
/// PT_LOAD (R+X, filesz=0, memsz=4 KiB @ 0x400000). Returns the header-region length.
unsafe fn build_base(nph: u16) -> usize {
    for b in FBUF.iter_mut() {
        *b = 0;
    }
    FBUF[0] = 0x7f;
    FBUF[1] = b'E';
    FBUF[2] = b'L';
    FBUF[3] = b'F';
    FBUF[4] = 2; // ELFCLASS64
    FBUF[5] = 1; // little-endian
    FBUF[6] = 1; // version
    w16(16, 2); // e_type = ET_EXEC
    w16(18, 62); // e_machine = x86_64
    w32(20, 1); // e_version
    w64(24, 0x400000); // e_entry
    w64(32, 64); // e_phoff
    w16(52, 64); // e_ehsize
    w16(54, 56); // e_phentsize
    w16(56, nph); // e_phnum
    let p0 = 64;
    w32(p0, 1); // PT_LOAD
    w32(p0 + 4, 5); // R+X
    w64(p0 + 16, 0x400000); // p_vaddr
    w64(p0 + 40, 0x1000); // p_memsz
    w64(p0 + 48, 0x1000); // p_align
    64 + (nph as usize) * 56
}
/// Spawn `FBUF[..len]`; true iff the kernel REJECTED it (Err) — the desired outcome.
unsafe fn rejected(len: usize) -> bool {
    let m = MsgBuf::new(0);
    rt::sys_spawn_bytes(&FBUF[..len], BOOT_MEM, &m, HANDLE_NULL).is_err()
}

/// Run the crafted-ELF battery; returns (rejected_count, total). Each mutation must be
/// refused by the loader.
fn elf_fuzz() -> (u32, u32) {
    let mut total = 0u32;
    let mut rej = 0u32;
    let mut t = |ok: bool| {
        total += 1;
        if ok {
            rej += 1;
        }
    };
    unsafe {
        let l = build_base(1);
        t(rejected(40)); // 1. truncated below the ELF header
        let l = build_base(1);
        FBUF[0] = 0;
        t(rejected(l)); // 2. bad magic
        let l = build_base(1);
        FBUF[4] = 1;
        t(rejected(l)); // 3. ELFCLASS32
        let l = build_base(1);
        w16(16, 3);
        t(rejected(l)); // 4. ET_DYN (PIE — unsupported)
        let l = build_base(1);
        w16(18, 0);
        t(rejected(l)); // 5. wrong machine
        let l = build_base(1);
        w16(56, 100);
        t(rejected(l)); // 6. phnum > MAX_PHDRS
        let l = build_base(1);
        w16(54, 32);
        t(rejected(l)); // 7. phentsize < sizeof(phdr)
        let l = build_base(1);
        w64(32, 0xF000);
        t(rejected(l)); // 8. phdr table past end
        let l = build_base(1);
        w32(64 + 4, 7);
        t(rejected(l)); // 9. W+X segment (L4)
        let l = build_base(1);
        w64(64 + 16, 0xFFFF_8000_0000_0000);
        t(rejected(l)); // 10. kernel-half p_vaddr
        let l = build_base(1);
        w64(64 + 16, 0xFFFF_FFFF_FFFF_F000);
        w64(64 + 40, 0x2000);
        t(rejected(l)); // 11. p_vaddr + p_memsz overflow
        let l = build_base(1);
        w64(64 + 16, 0x0F00_0000);
        t(rejected(l)); // 12. overlaps the reserved SPAWN_ARGV page
        let l = build_base(1);
        w64(64 + 8, 0xF000);
        t(rejected(l)); // 13. segment file range past the buffer
        let l = build_base(2);
        let p1 = 64 + 56;
        w32(p1, 7); // PT_TLS
        w64(p1 + 32, 0x4000); // p_filesz huge (the overflow we fixed)
        t(rejected(l)); // 14. PT_TLS filesz > memsz / past a frame
        let l = build_base(1);
        w64(64 + 32, 0x2000);
        w64(64 + 40, 0x1000);
        t(rejected(l)); // 15. p_filesz > p_memsz
    }
    (rej, total)
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    w(b"jail: a hostile program tries to escape its sandbox.\n");
    w(b"      holds only: one dir, stdout, a mem budget, net.\n");
    let mut held = 0u32;
    let mut total = 0u32;

    // L2 — handles are unforgeable. Guess an integer that names no capability of
    // ours; the kernel rejects it (a handle is a private table index, not a
    // global object id).
    total += 1;
    held += match rt::sys_attenuate(57, R_SEND) {
        Err(e) => report(b"[L2] forge a handle (guess #57)   ", true, errname(e)),
        Ok(h) => {
            let _ = rt::sys_close(h);
            report(b"[L2] forge a handle (guess #57)   ", false, b"")
        }
    };

    // L5 — attenuation only, never amplification. Mint our own endpoint (we own
    // it fully), attenuate to a subset that KEEPS the attenuate right, then ask to
    // widen it back. The kernel refuses to add a right.
    total += 1;
    held += match rt::sys_ep_create() {
        Ok(ep) => {
            let res = match rt::sys_attenuate(ep, R_SEND | R_ATTENUATE) {
                Ok(weak) => match rt::sys_attenuate(weak, R_SEND | R_RECV | R_ATTENUATE) {
                    Err(e) => report(b"[L5] amplify my own rights         ", true, errname(e)),
                    Ok(h) => {
                        let _ = rt::sys_close(h);
                        report(b"[L5] amplify my own rights         ", false, b"")
                    }
                },
                Err(e) => report(b"[L5] amplify my own rights         ", true, errname(e)),
            };
            let _ = rt::sys_close(ep);
            res
        }
        Err(e) => report(b"[L5] amplify my own rights         ", true, errname(e)),
    };

    // L1 — zero ambient authority. We hold no I/O-port capability; naming a handle
    // we DO hold (stdout, an endpoint) for port I/O is refused — a cap can't be
    // coerced into hardware access.
    total += 1;
    held += match rt::sys_io_in(SPAWN_STDOUT, 0x60) {
        Err(e) => report(b"[L1] read keyboard port 0x60       ", true, errname(e)),
        Ok(_) => report(b"[L1] read keyboard port 0x60       ", false, b""),
    };

    // L4 — W^X always. Map a writable scratch page from our budget, write to it,
    // then try to flip it to writable+executable. The kernel forbids W|X.
    total += 1;
    let scratch = 0x3000_0000u64;
    let scratch_ok = rt::sys_map(BOOT_MEM, scratch, 4096, PROT_READ | PROT_WRITE).is_ok();
    if scratch_ok {
        unsafe { core::ptr::write_volatile(scratch as *mut u8, 0xC3) }; // a 'ret'
        held += match rt::sys_protect(BOOT_MEM, scratch, 4096, PROT_READ | PROT_WRITE | PROT_EXEC) {
            Err(e) => report(b"[L4] make my code page W+X         ", true, errname(e)),
            Ok(_) => report(b"[L4] make my code page W+X         ", false, b""),
        };
    } else {
        held += report(b"[L4] make my code page W+X (no mem)", true, b"E_NOMEM");
    }

    // [H] mimmutable hardening: lock the scratch page immutable, then try a flip
    // that W^X WOULD permit (RW->RX). Immutability forbids it — the text can never
    // change again, even via a legal transition.
    total += 1;
    if scratch_ok {
        let _ = rt::sys_immutable(BOOT_MEM, scratch, 4096);
        held += match rt::sys_protect(BOOT_MEM, scratch, 4096, PROT_READ | PROT_EXEC) {
            Err(e) => report(b"[H ] re-protect an immutable page   ", true, errname(e)),
            Ok(_) => report(b"[H ] re-protect an immutable page   ", false, b""),
        };
    } else {
        held += report(b"[H ] immutable page (no mem)        ", true, b"E_NOMEM");
    }

    // L3 — no global namespace + capability confinement. We were handed a cap to
    // ONE directory. Prove we can act inside it (create a file), then prove we
    // cannot walk above it ('..' is rejected by the fs).
    total += 1;
    let inside = fs_call(DIR_CAP, TAG_FS_CREATE, b"jail-was-here");
    if inside == 0 {
        // Acting WITHIN the granted directory is allowed — the cap is real, not
        // just broken — which makes the escape denial below meaningful.
        w(b"  [L3] write INSIDE my cell          -> ALLOWED [ok]\n");
        held += 1;
    } else {
        w(b"  [L3] write INSIDE my cell          -> blocked?! [FAIL]\n");
    }
    total += 1;
    let escape = fs_call(DIR_CAP, TAG_FS_OPEN, b"../etc/passwd");
    held += report(b"[L3] escape via ../etc/passwd      ", escape != 0, b"no-such-path");

    // [PT] PENTEST 2026-06: namespace-control escape. TAG_FS_NAMESPACE used to be
    // UNAUTHENTICATED — ANY fs cap (even our /tmp cell) could mint a namespace rooted
    // at the disk root, at full RW, defeating ALL confinement. We try it, and if it
    // works, read /etc/passwd (far outside our cell) to prove the escape. The fix
    // gates namespace creation on the FS_ROOT authority cap, so we get DENIED.
    total += 1;
    let mut ns = MsgBuf::new(TAG_FS_NAMESPACE);
    unsafe { *(ns.data.as_mut_ptr() as *mut u8) = 0 }; // home = "" (the ext2 root)
    ns.data_len = 8;
    let got_ns = rt::sys_call(DIR_CAP, &mut ns).is_ok() && ns.data[0] == 0 && ns.handle_count >= 1;
    if !got_ns {
        held += report(b"[PT] mint a disk-root namespace    ", true, b"E_DENIED");
    } else {
        // We escaped the sandbox — now read a file we must never reach.
        let root_ns = ns.handles[0];
        let mut op = MsgBuf::new(TAG_FS_OPEN);
        let name = b"etc/passwd";
        let dst = op.data.as_mut_ptr() as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
            *dst.add(name.len()) = 0;
        }
        op.data_len = 8;
        let opened = rt::sys_call(root_ns, &mut op).is_ok() && op.data[0] == 0 && op.handle_count >= 1;
        if opened {
            let fcap = op.handles[0];
            let mut rd = MsgBuf::new(TAG_FS_READ);
            rd.data[0] = 0;
            rd.data_len = 1;
            let read = rt::sys_call(fcap, &mut rd).is_ok() && rd.data[0] > 0;
            let _ = rt::sys_close(fcap);
            if read {
                w(b"  [PT] mint root NS + read /etc/passwd-> ESCAPED, leaked bytes [LEAK!]\n");
            } else {
                w(b"  [PT] mint root NS, open /etc/passwd -> ESCAPED (opened) [LEAK!]\n");
            }
        } else {
            w(b"  [PT] mint a disk-root namespace    -> ESCAPED the cell [LEAK!]\n");
        }
        let _ = rt::sys_close(root_ns);
    }

    // L6 — memory accountability. Ask to map far more than our metered budget. The
    // kernel refuses; even memory is funded by a capability.
    total += 1;
    held += match rt::sys_map(BOOT_MEM, 0x3800_0000, 64 * 1024 * 1024, PROT_READ | PROT_WRITE) {
        Err(e) => report(b"[L6] map 64 MiB past my budget     ", true, errname(e)),
        Ok(_) => report(b"[L6] map 64 MiB past my budget     ", false, b""),
    };

    // [FUZZ] crafted-ELF rejection: the kernel's untrusted-image validator must refuse
    // every malformed ELF (no panic, no load). One executable regression for the whole
    // ELF-loader hardening arc.
    let (frej, ftot) = elf_fuzz();
    total += ftot;
    held += frej;
    w(b"  [FUZZ] crafted ELFs rejected: ");
    let mut fb = [0u8; 8];
    let fwrite = |v: u32, b: &mut [u8; 8]| -> usize {
        let mut i = 8;
        let mut x = v;
        loop {
            i -= 1;
            b[i] = b'0' + (x % 10) as u8;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        i
    };
    let fi = fwrite(frej, &mut fb);
    w(&fb[fi..]);
    w(b"/");
    let mut fb2 = [0u8; 8];
    let fi2 = fwrite(ftot, &mut fb2);
    w(&fb2[fi2..]);
    if frej == ftot {
        w(b" [ok]\n");
    } else {
        w(b" [LEAK! a malformed ELF was accepted]\n");
    }

    // Tally.
    w(b"jail: ");
    let mut nb = [0u8; 8];
    let fmt = |v: u32, b: &mut [u8; 8]| -> usize {
        let mut i = 8;
        let mut x = v;
        loop {
            i -= 1;
            b[i] = b'0' + (x % 10) as u8;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        i
    };
    let i = fmt(held, &mut nb);
    w(&nb[i..]);
    w(b"/");
    let mut nb2 = [0u8; 8];
    let j = fmt(total, &mut nb2);
    w(&nb2[j..]);
    if held == total {
        w(b" escape attempts blocked - confinement holds.\n");
    } else {
        w(b" blocked - A SANDBOX LEAK EXISTS!\n");
    }

    // Finale: pledge (defense-in-depth + fail-closed). Voluntarily renounce the
    // right to map memory (keep only stdio + ipc so we can still speak), then try
    // to map anyway. The kernel kills us the instant we break our own promise —
    // and only us: the shell prompt returns, every server keeps running.
    w(b"jail: finale - I pledge away MEM (keep stdio+ipc)...\n");
    let _ = rt::sys_pledge(PLEDGE_STDIO | PLEDGE_IPC);
    w(b"jail: pledged. now I try to map memory anyway:\n");
    // Let the tty flush before the pledge trips (preemption runs it during spin).
    for _ in 0..20_000_000u64 {
        core::hint::spin_loop();
    }
    let _ = rt::sys_map(BOOT_MEM, 0x4000_0000, 4096, PROT_READ | PROT_WRITE);

    w(b"jail: (this line should never appear)\n");
    rt::sys_exit(0)
}
