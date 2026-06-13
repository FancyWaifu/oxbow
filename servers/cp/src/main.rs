//! cp — copy a file. The first genuinely two-argument coreutil: it reads its
//! arguments as a real vector via `rt::args()`, and uses the libc file API
//! (open + read_all + create + write_all). The shell grants it the current
//! directory's capability at BOOT_EP and "src dst" as argv.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use oxbow_abi::{BOOT_EP, FS_FILE};
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let a: Vec<&[u8]> = rt::args().collect();
    if a.len() < 2 {
        rt::println!("cp: usage: cp <src> <dst>");
        rt::sys_exit(1);
    }
    let (src, dst) = (a[0], a[1]);
    match rt::fs::open(BOOT_EP, src) {
        Some(node) if node.kind == FS_FILE => {
            let data = rt::fs::read_all(node.cap);
            match rt::fs::create(BOOT_EP, dst) {
                Some(file) => rt::fs::write_all(file, &data),
                None => rt::println!("cp: cannot create {}", String::from_utf8_lossy(dst)),
            }
        }
        _ => rt::println!("cp: {}: no such file", String::from_utf8_lossy(src)),
    }
    rt::sys_exit(0)
}
