//! mv — a spawnable coreutil. The shell grants the current directory's capability
//! at slot 1 (BOOT_EP) and "<old> <new>" as argv; mv splits the two names and
//! issues RENAME (rename within the directory). Cross-directory moves (two dir
//! caps) are deferred.
#![no_std]
#![no_main]

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

/// Split off the first space-delimited token; returns (token, rest-trimmed).
fn token(s: &[u8]) -> (&[u8], &[u8]) {
    let mut a = 0;
    while a < s.len() && s[a] == b' ' {
        a += 1;
    }
    let mut e = a;
    while e < s.len() && s[e] != b' ' {
        e += 1;
    }
    let mut r = e;
    while r < s.len() && s[r] == b' ' {
        r += 1;
    }
    (&s[a..e], &s[r..])
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let arg = rt::argv();
    let (old, rest) = token(arg);
    let (new, _) = token(rest);
    if old.is_empty() || new.is_empty() {
        tw(b"mv: usage: mv <old> <new>\n");
        rt::sys_exit(1);
    }
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
