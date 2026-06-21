//! kill — terminate a process by PID via SYS_KILL (pledge-gated, PLEDGE_PROC). A
//! leading -SIGNAL is accepted but ignored: oxbow has no signal delivery, so kill
//! always terminates. The capability-pure way to kill your own child is its
//! exit-notif cap (SYS_PROC_KILL); this is the ambient `kill <pid>` users expect.
#![no_std]
#![no_main]

use oxbow_rt as rt;

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let mut args = rt::args();
    let mut a = args.next();
    if let Some(t) = a {
        if t.first() == Some(&b'-') {
            a = args.next(); // skip a -SIGNAL token
        }
    }
    let pid = match a.and_then(parse_u32) {
        Some(p) => p,
        None => {
            rt::println!("usage: kill [-signal] pid");
            rt::sys_exit(1);
        }
    };
    match rt::sys_kill(pid, 9) {
        Ok(_) => rt::sys_exit(0),
        Err(_) => {
            rt::println!("kill: ({}): no such process", pid);
            rt::sys_exit(1);
        }
    }
}
