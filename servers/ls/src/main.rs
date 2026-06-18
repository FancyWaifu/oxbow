//! ls — written against the oxbow-rt "libc". The shell hands ls the current
//! directory capability at BOOT_EP (slot 1) and the optional target path as argv.
//! With no arg ls lists the cwd; with a path it OPENS it relative to the cwd cap
//! (so ls resolves its own argument — it is a plain /bin file, not a builtin the
//! shell pre-resolves). Entries print one per line (directories get a `/`).
#![no_std]
#![no_main]

extern crate alloc;

use oxbow_abi::{BOOT_EP, FS_DIR};
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // The first argv token (if any) is the directory to list, relative to our cwd.
    let dir = match rt::args().next() {
        None => BOOT_EP,
        Some(p) => match rt::fs::open(BOOT_EP, p) {
            Some(n) if n.kind == FS_DIR => n.cap,
            Some(_) => {
                rt::stdout_write(b"ls: ");
                rt::stdout_write(p);
                rt::println!(": not a directory");
                rt::sys_exit(1)
            }
            None => {
                rt::stdout_write(b"ls: ");
                rt::stdout_write(p);
                rt::println!(": no such directory");
                rt::sys_exit(1)
            }
        },
    };
    let mut i = 0u64;
    while let Some((name, kind)) = rt::fs::readdir(dir, i) {
        let suffix = if kind == FS_DIR { "/" } else { "" };
        rt::stdout_write(&name);
        rt::println!("{}", suffix);
        i += 1;
    }
    rt::sys_exit(0)
}
