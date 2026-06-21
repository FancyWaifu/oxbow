//! spin — a do-nothing process that loops forever (yielding). A kill target for
//! testing ps/kill: run `spin &`, find its PID with `ps`, then `kill <pid>`.
#![no_std]
#![no_main]

use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    loop {
        rt::sys_yield();
    }
}
