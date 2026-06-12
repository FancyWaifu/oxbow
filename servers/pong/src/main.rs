//! pong — the first oxbow user-mode server.
//!
//! The v0 acceptance trace (ABI §7 steps 6-10): call the boot endpoint with
//! PING, get PONG back in the same buffer, and print it through the console
//! capability. With `--features selftest`, first run the ABI negative-path
//! tests, deliberately tripping each documented error from ring 3.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_CONSOLE, BOOT_EP, TAG_PONG, TAG_PING};
use oxbow_rt as rt;

/// Write a byte string through the console capability.
fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
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
    #[cfg(feature = "selftest")]
    selftest();

    let mut msg = MsgBuf::new(TAG_PING);
    match rt::sys_call(BOOT_EP, &mut msg) {
        // The reply landed in the same buffer; data[0] holds "PONG\n\0\0\0".
        Ok(()) if msg.tag == TAG_PONG => {
            let _ = rt::sys_console_write(BOOT_CONSOLE, msg.data.as_ptr() as *const u8, 5);
        }
        _ => w(b"call failed\n"),
    }

    // Spin in ring 3 (IF=1) so the timer can visibly preempt us — printing a
    // `u` between bursts of pure user-mode compute. Proves the user thread
    // survives many preemptions before exiting.
    for _ in 0..6 {
        w(b"u ");
        let mut x: u64 = 0;
        for i in 0..3_000_000u64 {
            x = x.wrapping_add(i);
        }
        core::hint::black_box(x);
    }
    w(b"\n");

    rt::sys_exit(0)
}
