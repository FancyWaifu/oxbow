//! mkdir — a spawnable coreutil that takes a NAME via argv. The shell grants it
//! the current directory's capability at slot 1 (BOOT_EP) and passes the new
//! directory's name as the spawn argument; mkdir reads `rt::argv()` and issues
//! MKDIR. It needs a name (the directory doesn't exist yet to be handed as a
//! cap) — which is exactly what argv is for.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_EP, SPAWN_STDOUT, TAG_FS_MKDIR, TAG_TTY_WRITE};
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
        tw(b"mkdir: usage: mkdir <name>\n");
        rt::sys_exit(1);
    }
    let mut m = MsgBuf::new(TAG_FS_MKDIR);
    let n = core::cmp::min(name.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    if rt::sys_call(BOOT_EP, &mut m).is_err() || m.data[0] != 0 {
        tw(b"mkdir: cannot create ");
        tw(name);
        tw(b"\n");
    }
    rt::sys_exit(0)
}
