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
//! table proving laws L1–L6 (docs/abi-v0.md §1) hold. As a finale it touches
//! memory it does not own, faults, and dies ALONE: the machine survives and the
//! shell prompt returns.
#![no_std]
#![no_main]

use oxbow_abi::{
    Handle, MsgBuf, SysError, BOOT_MEM, PROT_EXEC, PROT_READ, PROT_WRITE, R_ATTENUATE, R_RECV,
    R_SEND, SPAWN_STDOUT, TAG_FS_CREATE, TAG_FS_OPEN, TAG_TTY_WRITE,
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
    if rt::sys_map(BOOT_MEM, scratch, 4096, PROT_READ | PROT_WRITE).is_ok() {
        unsafe { core::ptr::write_volatile(scratch as *mut u8, 0xC3) }; // a 'ret'
        held += match rt::sys_protect(BOOT_MEM, scratch, 4096, PROT_READ | PROT_WRITE | PROT_EXEC) {
            Err(e) => report(b"[L4] make my code page W+X         ", true, errname(e)),
            Ok(_) => report(b"[L4] make my code page W+X         ", false, b""),
        };
    } else {
        held += report(b"[L4] make my code page W+X (no mem)", true, b"E_NOMEM");
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

    // L6 — memory accountability. Ask to map far more than our metered budget. The
    // kernel refuses; even memory is funded by a capability.
    total += 1;
    held += match rt::sys_map(BOOT_MEM, 0x3800_0000, 64 * 1024 * 1024, PROT_READ | PROT_WRITE) {
        Err(e) => report(b"[L6] map 64 MiB past my budget     ", true, errname(e)),
        Ok(_) => report(b"[L6] map 64 MiB past my budget     ", false, b""),
    };

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

    // Finale: fault isolation. Touch memory we don't own. The kernel kills only
    // this process; the shell prompt returns and every server keeps running.
    w(b"jail: finale - dereferencing an address I was never granted...\n");
    // Give the tty a moment to flush the line before we fault (preemption lets it
    // run during the spin).
    for _ in 0..20_000_000u64 {
        core::hint::spin_loop();
    }
    unsafe { core::ptr::write_volatile(0xDEAD_0000 as *mut u64, 0) };

    w(b"jail: (this line should never appear)\n");
    rt::sys_exit(0)
}
