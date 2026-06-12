//! pong — the first oxbow user-mode server.
//!
//! The v0 acceptance trace (ABI §7 steps 6-10): call the boot endpoint with
//! PING, get PONG back in the same buffer, and print it through the console
//! capability. With `--features selftest`, first run the ABI negative-path
//! tests, deliberately tripping each documented error from ring 3.
#![no_std]
#![no_main]

use oxbow_abi::{
    MsgBuf, SysError, BOOT_CONSOLE, BOOT_EP, BOOT_MEM, PROT_READ, PROT_WRITE, R_GRANT, R_MAP,
    TAG_PONG, TAG_PING,
};
use oxbow_rt as rt;

/// Write a byte string through the console capability.
fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Print the byte `v` as two hex digits.
#[cfg(feature = "isolation")]
fn hex_byte(v: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    w(&[HEX[(v >> 4) as usize], HEX[(v & 0xf) as usize]]);
}

/// Exercise the ABI's failure modes — each should be rejected with the exact
/// documented error, all enforced by the kernel against an unprivileged caller.
#[cfg(feature = "selftest")]
fn selftest() {
    use oxbow_abi::{SysError, R_SEND, R_WRITE};
    let mut pass: u32 = 0;

    // 1. Unknown handle → E_BAD_HANDLE.
    {
        let mut m = MsgBuf::new(TAG_PING);
        if rt::sys_call(9, &mut m) == Err(SysError::BadHandle) {
            pass += 1;
        }
    }
    // 2. recv on an endpoint we hold without R_RECV → E_RIGHTS.
    {
        let mut m = MsgBuf::new(0);
        if rt::sys_recv(BOOT_EP, &mut m) == Err(SysError::Rights) {
            pass += 1;
        }
    }
    // 3. Attenuation that ADDS a right (amplification) → E_RIGHTS.
    if rt::sys_attenuate(BOOT_CONSOLE, R_WRITE | R_SEND) == Err(SysError::Rights) {
        pass += 1;
    }
    // 4. A kernel (higher-half) pointer → E_FAULT.
    if rt::sys_console_write(BOOT_CONSOLE, 0xffff_ffff_8000_0000u64 as *const u8, 8)
        == Err(SysError::Fault)
    {
        pass += 1;
    }
    // 5. Console write longer than the 1024-byte limit → E_MSG.
    {
        let b = [b'x'];
        if rt::sys_console_write(BOOT_CONSOLE, b.as_ptr(), 2000) == Err(SysError::Msg) {
            pass += 1;
        }
    }
    // 6. A MsgBuf claiming more data words than exist → E_MSG.
    {
        let mut m = MsgBuf::new(TAG_PING);
        m.data_len = 99;
        if rt::sys_call(BOOT_EP, &mut m) == Err(SysError::Msg) {
            pass += 1;
        }
    }
    // 7. An unknown syscall number → E_NOSYS.
    if rt::sys_raw(99, 0, 0, 0).0 == SysError::Nosys as u64 {
        pass += 1;
    }

    w(b"selftest: ");
    w(&[b'0' + pass as u8]);
    w(b"/7 ok\n");
}

/// Server entry, called by oxbow-rt's `_start`.
#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // Fault-containment test: touch an unmapped user address. The kernel should
    // kill this thread/process and keep running everything else.
    #[cfg(feature = "faulttest")]
    unsafe {
        core::ptr::read_volatile(0x10 as *const u64);
    }

    #[cfg(feature = "isolation")]
    {
        // Read our OWN 8 bytes at 0x200000 — different from beta's, same vaddr.
        w(b"[P1] @0x200000 = ");
        for i in 0..8 {
            let b = unsafe { core::ptr::read_volatile((0x200000 + i) as *const u8) };
            hex_byte(b);
        }
        w(b"\n");
    }

    #[cfg(feature = "selftest")]
    selftest();

    // Call the boot endpoint with PING across three rounds. With the kernel echo
    // gone, the reply comes from beta (the ponger) in its own address space.
    // Round 1 is sender-first, rounds 2-3 receiver-first; round 3 sees beta die
    // mid-call, so the kernel returns E_GONE.
    // v1 memory arc: map 16 KiB of anonymous memory from our Memory budget,
    // write a per-address pattern, read it back, and verify.
    const HEAP: u64 = 0x4000_0000;
    const HEAP_LEN: u64 = 16 * 1024;
    match rt::sys_map(BOOT_MEM, HEAP, HEAP_LEN, PROT_READ | PROT_WRITE) {
        Ok(()) => {
            let mut ok = true;
            let mut a = HEAP;
            while a < HEAP + HEAP_LEN {
                unsafe { core::ptr::write_volatile(a as *mut u64, a ^ 0xA5A5_A5A5_A5A5_A5A5) };
                a += 8;
            }
            let mut a = HEAP;
            while a < HEAP + HEAP_LEN {
                if unsafe { core::ptr::read_volatile(a as *const u64) } != (a ^ 0xA5A5_A5A5_A5A5_A5A5)
                {
                    ok = false;
                    break;
                }
                a += 8;
            }
            if ok {
                w(b"[P1] mapped 16 KiB @ 0x40000000, pattern verified\n");
            } else {
                w(b"[P1] map verify FAILED\n");
            }
        }
        Err(_) => w(b"[P1] sys_map failed\n"),
    }

    // Shared memory: mint a Frame, map it writable, write a message, then
    // attenuate to a READ-ONLY grantable handle and hand it to the ponger in
    // round 1's PING — zero-copy data across two isolated address spaces.
    const SHARED: u64 = 0x5000_0000;
    let ro_frame = match rt::sys_frame_alloc(BOOT_MEM) {
        Ok(f) => {
            let _ = rt::sys_frame_map(f, SHARED, PROT_READ | PROT_WRITE);
            let m = b"HELLO ACROSS AS\0";
            unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), SHARED as *mut u8, m.len()) };
            // Drop R_WRITE + R_ATTENUATE; keep R_MAP|R_GRANT — a read-only leaf.
            rt::sys_attenuate(f, R_MAP | R_GRANT).unwrap_or(f)
        }
        Err(_) => {
            w(b"[P1] frame alloc failed\n");
            0
        }
    };

    for round in 1u8..=3 {
        let mut msg = MsgBuf::new(TAG_PING);
        if round == 1 && ro_frame != 0 {
            msg.handle_count = 1;
            msg.handles[0] = ro_frame;
        }
        match rt::sys_call(BOOT_EP, &mut msg) {
            Ok(()) if msg.tag == TAG_PONG => {
                w(b"[P1] round ");
                w(&[b'0' + round]);
                w(b": ");
                let _ = rt::sys_console_write(BOOT_CONSOLE, msg.data.as_ptr() as *const u8, 5);
            }
            Err(SysError::Gone) => {
                w(b"[P1] round ");
                w(&[b'0' + round]);
                w(b" -> E_GONE ok\n");
            }
            _ => {
                w(b"[P1] round ");
                w(&[b'0' + round]);
                w(b" unexpected\n");
            }
        }
    }

    rt::sys_exit(0)
}
