//! touch — a spawnable coreutil that creates an empty file named by argv. The
//! shell grants the current directory's capability at slot 1 (BOOT_EP) and the
//! file name as the spawn argument; touch reads `rt::argv()` and issues CREATE
//! (create-or-truncate, then closes the returned cap without writing).
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_EP, SPAWN_STDOUT, TAG_FS_CREATE, TAG_TTY_WRITE};
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
    let name = rt::argv();
    if name.is_empty() {
        tw(b"touch: usage: touch <name>\n");
        rt::sys_exit(1);
    }
    let mut m = MsgBuf::new(TAG_FS_CREATE);
    let n = core::cmp::min(name.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    if rt::sys_call(BOOT_EP, &mut m).is_err() || m.data[0] != 0 {
        tw(b"touch: cannot create ");
        tw(name);
        tw(b"\n");
    } else {
        let _ = rt::sys_close(m.handles[0]); // we don't write — just create
    }
    rt::sys_exit(0)
}
