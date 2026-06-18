//! cat — written against the oxbow-rt "libc". Two modes:
//!   `cat <file>`  the shell resolves the name and hands cat the FILE capability
//!                 at BOOT_EP (slot 1); cat reads the whole file and writes it.
//!   `cat -`       a pipeline consumer (§81): read stdin (the pipe read end the
//!                 shell granted at SPAWN_STDIN) until EOF, echoing it to stdout.
#![no_std]
#![no_main]

extern crate alloc;

use oxbow_abi::BOOT_EP;
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // `-` selects stdin mode (a `… | cat` pipeline); otherwise read the file cap.
    let stdin_mode = rt::args().any(|a| a == b"-");
    if stdin_mode {
        let mut buf = [0u8; 256];
        loop {
            let n = rt::stdin_read(&mut buf);
            if n == 0 {
                break; // write side closed: end of input
            }
            rt::stdout_write(&buf[..n]);
        }
    } else {
        let data = rt::fs::read_all(BOOT_EP);
        rt::stdout_write(&data);
    }
    rt::sys_exit(0)
}
