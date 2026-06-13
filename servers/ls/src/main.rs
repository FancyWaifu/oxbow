//! ls — written against the oxbow-rt "libc". The shell hands ls a DIRECTORY
//! capability at BOOT_EP (slot 1); ls iterates its entries with `fs::readdir`
//! and `println!`s each (directories marked with a trailing '/').
#![no_std]
#![no_main]

extern crate alloc;

use oxbow_abi::{BOOT_EP, FS_DIR};
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let mut i = 0u64;
    while let Some((name, kind)) = rt::fs::readdir(BOOT_EP, i) {
        let suffix = if kind == FS_DIR { "/" } else { "" };
        // name is raw bytes; print via the fmt path as lossy-utf8 is fine for ASCII
        rt::stdout_write(&name);
        rt::println!("{}", suffix);
        i += 1;
    }
    rt::sys_exit(0)
}
