//! oxbow stdio — routes std's Stdin/Stdout/Stderr to oxbow-rt's hosted console
//! shims (`__oxbow_write`/`__oxbow_read`, backed by SYS_CONSOLE_WRITE).
use crate::io;

unsafe extern "C" {
    fn __oxbow_write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn __oxbow_read(fd: i32, buf: *mut u8, len: usize) -> isize;
}

fn do_write(fd: i32, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe { __oxbow_write(fd, buf.as_ptr(), buf.len()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin { Stdin }
}
impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { __oxbow_read(0, buf.as_mut_ptr(), buf.len()) };
        if n < 0 { Ok(0) } else { Ok(n as usize) }
    }
}

impl Stdout {
    pub const fn new() -> Stdout { Stdout }
}
impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> { do_write(1, buf) }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Stderr {
    pub const fn new() -> Stderr { Stderr }
}
impl io::Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> { do_write(2, buf) }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

pub const STDIN_BUF_SIZE: usize = 1024;
pub fn is_ebadf(_err: &io::Error) -> bool { false }
pub fn panic_output() -> Option<Vec<u8>> { Some(Vec::new()) }
