//! cat — written against the oxbow-rt "libc". The shell resolves the name and
//! hands cat the FILE capability at BOOT_EP (slot 1); cat reads the whole file
//! into a Vec and writes it to stdout. Compare the old hand-packed READ loop.
#![no_std]
#![no_main]

extern crate alloc;

use oxbow_abi::BOOT_EP;
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let data = rt::fs::read_all(BOOT_EP);
    rt::stdout_write(&data);
    rt::sys_exit(0)
}
