//! cat — written against the oxbow-rt "libc". The shell hands cat the current
//! directory capability at BOOT_EP (slot 1) and the file name(s) as argv; cat
//! OPENS each relative to the cwd cap and writes it (so cat resolves its own
//! arguments — a plain /bin file, not a builtin the shell pre-resolves). With no
//! file argument (or `-`) it reads stdin until EOF: the pipeline consumer form
//! (`… | cat`, §81) and `cat < file`.
#![no_std]
#![no_main]

extern crate alloc;

use oxbow_abi::{BOOT_EP, FS_FILE};
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let mut status = 0;
    let mut had_file = false;
    for a in rt::args() {
        if a == b"-" {
            continue; // explicit stdin marker — handled below if no files given
        }
        had_file = true;
        match rt::fs::open(BOOT_EP, a) {
            Some(n) if n.kind == FS_FILE => {
                let data = rt::fs::read_all(n.cap);
                rt::stdout_write(&data);
            }
            Some(_) => {
                rt::stdout_write(b"cat: ");
                rt::stdout_write(a);
                rt::println!(": is a directory");
                status = 1;
            }
            None => {
                rt::stdout_write(b"cat: ");
                rt::stdout_write(a);
                rt::println!(": not found");
                status = 1;
            }
        }
    }
    if !had_file {
        // No file argument: stream stdin (the pipe read end, or `< file`) to stdout.
        let mut buf = [0u8; 256];
        loop {
            let n = rt::stdin_read(&mut buf);
            if n == 0 {
                break;
            }
            rt::stdout_write(&buf[..n]);
        }
    }
    rt::sys_exit(status)
}
