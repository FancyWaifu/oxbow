//! rm — a spawnable coreutil. The shell grants the current directory's capability
//! at slot 1 (BOOT_EP) and the target name as argv; rm issues UNLINK. Removes a
//! file or an empty directory (no `-r`). It can only touch the one directory it
//! was handed — confinement holds for destructive ops too.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_EP, SPAWN_STDOUT, TAG_FS_UNLINK, TAG_TTY_WRITE};
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
        tw(b"rm: usage: rm <name>\n");
        rt::sys_exit(1);
    }
    let mut m = MsgBuf::new(TAG_FS_UNLINK);
    let n = core::cmp::min(name.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
    let ok = rt::sys_call(BOOT_EP, &mut m).is_ok();
    match (ok, m.data[0]) {
        (true, 0) => {}
        (true, 2) => {
            tw(b"rm: ");
            tw(name);
            tw(b": directory not empty\n");
        }
        _ => {
            tw(b"rm: ");
            tw(name);
            tw(b": no such file\n");
        }
    }
    rt::sys_exit(0)
}
