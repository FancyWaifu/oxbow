//! hello — the simplest spawnable program, now written against the oxbow-rt
//! "libc": a real heap (Vec/String/format!) and `println!` to stdout. Compare to
//! the old hand-packed MsgBuf version — this is what programs look like now.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let squares: Vec<u32> = (1..=4).map(|n| n * n).collect();
    rt::println!("hello, world");
    rt::println!("from the oxbow libc: squares {:?}", squares);
    rt::sys_exit(0)
}
