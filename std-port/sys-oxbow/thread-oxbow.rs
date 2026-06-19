//! oxbow threads — std::thread on SYS_THREAD_SPAWN/EXIT + the futex (via oxbow-rt
//! hosted shims). The spawned thread runs `init.init()` then signals a join word;
//! `join` futex-waits on it. The kernel sets the join word AFTER the thread is off
//! its user stack (SYS_THREAD_EXIT), so `join` can free that stack without racing.
use crate::ffi::CStr;
use crate::io;
use crate::num::NonZero;
use crate::sync::atomic::{AtomicU32, Ordering};
use crate::thread::ThreadInit;
use crate::time::Duration;

unsafe extern "C" {
    fn __oxbow_thread_spawn(stack_top: u64, entry: extern "C" fn(u64) -> !, arg: u64) -> u64;
    fn __oxbow_thread_exit(done_addr: u64) -> !;
    fn __oxbow_thread_id() -> u64;
    fn __oxbow_yield();
    fn __oxbow_uptime_ms() -> u64;
    fn __oxbow_futex_wait(addr: *const u32, expected: u32);
}

pub const DEFAULT_MIN_STACK_SIZE: usize = 256 * 1024;

struct Packet {
    init: Box<ThreadInit>,
    done: *const AtomicU32,
}

pub struct Thread {
    done: *const AtomicU32,
    stack: *mut u8,
    stack_len: usize,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

extern "C" fn thread_start(arg: u64) -> ! {
    // Reconstruct the packet, set up this thread (sets the per-thread CURRENT TLS),
    // run the user closure, then exit with the join signal.
    let packet = unsafe { Box::from_raw(arg as *mut Packet) };
    let done = packet.done as u64;
    let run = packet.init.init();
    run();
    unsafe { __oxbow_thread_exit(done) }
}

impl Thread {
    // unsafe: see thread::Builder::spawn_unchecked
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let stack_len = stack.max(DEFAULT_MIN_STACK_SIZE).next_multiple_of(16);
        let layout = crate::alloc::Layout::from_size_align(stack_len, 16).unwrap();
        let stack_ptr = unsafe { crate::alloc::alloc(layout) };
        if stack_ptr.is_null() {
            return Err(io::Error::from(io::ErrorKind::OutOfMemory));
        }
        let done = Box::into_raw(Box::new(AtomicU32::new(0)));
        let packet = Box::into_raw(Box::new(Packet { init, done }));
        let stack_top = stack_ptr as u64 + stack_len as u64;
        unsafe { __oxbow_thread_spawn(stack_top, thread_start, packet as u64) };
        Ok(Thread { done, stack: stack_ptr, stack_len })
    }

    pub fn join(self) {
        let done = unsafe { &*self.done };
        while done.load(Ordering::Acquire) == 0 {
            unsafe { __oxbow_futex_wait(self.done as *const u32, 0) };
        }
        // The kernel set the join word with the thread already off its stack, so
        // reclaiming the stack + join word here is safe.
        unsafe {
            let layout = crate::alloc::Layout::from_size_align(self.stack_len, 16).unwrap();
            crate::alloc::dealloc(self.stack, layout);
            drop(Box::from_raw(self.done as *mut AtomicU32));
        }
    }
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    Ok(NonZero::new(1).unwrap())
}

pub fn current_os_id() -> Option<u64> {
    Some(unsafe { __oxbow_thread_id() })
}

pub fn yield_now() {
    unsafe { __oxbow_yield() };
}

pub fn set_name(_name: &CStr) {}

pub fn sleep(dur: Duration) {
    let ms = dur.as_millis() as u64;
    let start = unsafe { __oxbow_uptime_ms() };
    while unsafe { __oxbow_uptime_ms() }.wrapping_sub(start) < ms {
        unsafe { __oxbow_yield() };
    }
}
