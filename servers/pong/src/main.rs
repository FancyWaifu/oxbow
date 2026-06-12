//! pong — the first oxbow user-mode server.
//!
//! The v0 acceptance trace (ABI §7 steps 6-10): call the boot endpoint with
//! PING, get PONG back in the same buffer, and print it through the console
//! capability. With `--features selftest`, first run the ABI negative-path
//! tests, deliberately tripping each documented error from ring 3.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, SysError, BOOT_CONSOLE, BOOT_EP, R_GRANT, R_WRITE, TAG_PONG, TAG_PING};
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
    // Derive a write-only, grantable console handle to hand to the ponger in
    // round 1's PING — a real capability moving across the rendezvous (§3.4).
    let granted = rt::sys_attenuate(BOOT_CONSOLE, R_WRITE | R_GRANT).unwrap_or(BOOT_CONSOLE);

    for round in 1u8..=3 {
        let mut msg = MsgBuf::new(TAG_PING);
        if round == 1 {
            msg.handle_count = 1;
            msg.handles[0] = granted;
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
