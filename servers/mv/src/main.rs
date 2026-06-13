//! mv — rename within a directory. Reads its two arguments via `rt::args()` (a
//! real argv vector) and issues RENAME through the directory capability the shell
//! granted at BOOT_EP. Cross-directory moves are supported by the fs (multi-
//! component paths, §15.10).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use oxbow_abi::{MsgBuf, BOOT_EP, SPAWN_STDOUT, TAG_FS_RENAME, TAG_TTY_WRITE};
use oxbow_rt as rt;

fn tw(s: &[u8]) {
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

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let a: Vec<&[u8]> = rt::args().collect();
    if a.len() < 2 {
        tw(b"mv: usage: mv <old> <new>\n");
        rt::sys_exit(1);
    }
    let (old, new) = (a[0], a[1]);
    // Pack old NUL new NUL into the request data.
    let mut m = MsgBuf::new(TAG_FS_RENAME);
    let dst = m.data.as_mut_ptr() as *mut u8;
    let ol = core::cmp::min(old.len(), 28);
    let nl = core::cmp::min(new.len(), 28);
    unsafe {
        core::ptr::copy_nonoverlapping(old.as_ptr(), dst, ol);
        *dst.add(ol) = 0;
        core::ptr::copy_nonoverlapping(new.as_ptr(), dst.add(ol + 1), nl);
        *dst.add(ol + 1 + nl) = 0;
    }
    m.data_len = 8;
    if rt::sys_call(BOOT_EP, &mut m).is_err() || m.data[0] != 0 {
        tw(b"mv: cannot rename ");
        tw(old);
        tw(b"\n");
    }
    rt::sys_exit(0)
}
