//! ls — a spawnable coreutil. The shell grants it a DIRECTORY capability at slot 1
//! (BOOT_EP); ls loops READDIR over it and writes each entry to stdout (slot 2).
//! It holds only that one directory, read-only — no namespace, no other files.
#![no_std]
#![no_main]

use oxbow_abi::{MsgBuf, BOOT_EP, FS_DIR, SPAWN_STDOUT, TAG_FS_READDIR, TAG_TTY_WRITE};
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
    let mut i = 0u64;
    loop {
        let mut m = MsgBuf::new(TAG_FS_READDIR);
        m.data[0] = i;
        m.data_len = 1;
        if rt::sys_call(BOOT_EP, &mut m).is_err() || m.data[0] == 0 {
            break;
        }
        let bytes = unsafe { core::slice::from_raw_parts((m.data.as_ptr() as *const u8).add(16), 48) };
        let n = bytes.iter().position(|&b| b == 0).unwrap_or(0);
        tw(&bytes[..n]);
        if m.data[1] == FS_DIR {
            tw(b"/");
        }
        tw(b"\n");
        i += 1;
    }
    rt::sys_exit(0)
}
