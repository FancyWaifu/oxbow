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
    // §101 TLS destructors: oxbow has no automatic thread-exit callback (its
    // `guard::enable` is a no-op), so run this thread's registered TLS destructors
    // and the runtime's per-thread cleanup here, before signalling join + exiting.
    unsafe { crate::sys::thread_local::destructors::run() };
    crate::rt::thread_cleanup();
    unsafe { __oxbow_thread_exit(done) }
}

// §104: a thread's user stack is heap memory the kernel does not reclaim, so it
// must be freed once the thread has left it. `join` waits and `Drop` frees (the
// thread sets its join word AFTER the kernel carried it off the stack). A DETACHED
// thread (handle dropped while it still runs) can't be freed yet — it's parked here
// and reaped on a later spawn, once its join word shows it exited. This stops the
// per-thread stack leak that OOM'd thread-heavy programs (e.g. a libtest runner).
struct Reapable {
    stack: usize,
    stack_len: usize,
    done: usize,
}
static REAPER: crate::sync::Mutex<crate::vec::Vec<Reapable>> =
    crate::sync::Mutex::new(crate::vec::Vec::new());

unsafe fn free_thread(stack: *mut u8, stack_len: usize, done: *const AtomicU32) {
    unsafe {
        let layout = crate::alloc::Layout::from_size_align(stack_len, 16).unwrap();
        crate::alloc::dealloc(stack, layout);
        drop(Box::from_raw(done as *mut AtomicU32));
    }
}

fn reap_detached() {
    let mut r = REAPER.lock().unwrap_or_else(|e| e.into_inner());
    r.retain(|d| {
        let done = unsafe { &*(d.done as *const AtomicU32) };
        if done.load(Ordering::Acquire) != 0 {
            unsafe { free_thread(d.stack as *mut u8, d.stack_len, d.done as *const AtomicU32) };
            false
        } else {
            true
        }
    });
}

impl Drop for Thread {
    fn drop(&mut self) {
        let done = unsafe { &*self.done };
        if done.load(Ordering::Acquire) != 0 {
            unsafe { free_thread(self.stack, self.stack_len, self.done) };
        } else {
            // Still running: hand it to the reaper to free after it exits.
            REAPER.lock().unwrap_or_else(|e| e.into_inner()).push(Reapable {
                stack: self.stack as usize,
                stack_len: self.stack_len,
                done: self.done as usize,
            });
        }
    }
}

impl Thread {
    // unsafe: see thread::Builder::spawn_unchecked
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        reap_detached(); // sweep exited detached threads' stacks before allocating
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
        // `self` drops here: the join word is set, so `Drop` reclaims the stack +
        // join word (the kernel carried the thread off its stack before setting it).
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
