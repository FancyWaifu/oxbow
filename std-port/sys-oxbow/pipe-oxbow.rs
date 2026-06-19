//! oxbow anonymous pipe — over SYS_PIPE (rt shims). A pipe is one kernel object
//! attenuated into a write-end (R_OUT|R_GRANT, handed to a child as stdout) and a
//! read-end (R_IN, read by the parent). EOF when all write-ends close (child exit).
use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};

unsafe extern "C" {
    fn __oxbow_pipe(rend_out: *mut u32, wend_out: *mut u32) -> i32;
    fn __oxbow_pipe_read(pipe: u32, buf: *mut u8, len: usize) -> isize;
    fn __oxbow_pipe_write(pipe: u32, buf: *const u8, len: usize) -> isize;
    fn __oxbow_pipe_close(pipe: u32);
}

pub struct Pipe(u32);

impl Pipe {
    pub(crate) fn handle(&self) -> u32 {
        self.0
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "oxbow: pipe try_clone"))
    }
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { __oxbow_pipe_read(self.0, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            Err(io::Error::new(io::ErrorKind::Other, "oxbow: pipe read"))
        } else {
            Ok(n as usize)
        }
    }
    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let mut tmp = [0u8; 512];
        let want = cursor.capacity().min(tmp.len());
        let n = self.read(&mut tmp[..want])?;
        cursor.append(&tmp[..n]);
        Ok(())
    }
    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        for b in bufs {
            if !b.is_empty() {
                return self.read(b);
            }
        }
        Ok(0)
    }
    pub fn is_read_vectored(&self) -> bool {
        false
    }
    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let mut tmp = [0u8; 512];
            let n = self.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            total += n;
        }
        Ok(total)
    }
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { __oxbow_pipe_write(self.0, buf.as_ptr(), buf.len()) };
        if n < 0 {
            Err(io::Error::new(io::ErrorKind::Other, "oxbow: pipe write"))
        } else {
            Ok(n as usize)
        }
    }
    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        for b in bufs {
            if !b.is_empty() {
                return self.write(b);
            }
        }
        Ok(0)
    }
    pub fn is_write_vectored(&self) -> bool {
        false
    }
    pub fn diverge(&self) -> ! {
        panic!("oxbow: pipe diverge (unsupported pipe chaining)")
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe { __oxbow_pipe_close(self.0) };
    }
}

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe").field("handle", &self.0).finish()
    }
}

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let mut rend = 0u32;
    let mut wend = 0u32;
    if unsafe { __oxbow_pipe(&mut rend, &mut wend) } != 0 {
        return Err(io::Error::new(io::ErrorKind::Other, "oxbow: pipe create"));
    }
    Ok((Pipe(rend), Pipe(wend)))
}
