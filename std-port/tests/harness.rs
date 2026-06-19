#![no_main]
#![feature(custom_test_frameworks)]
#![feature(thread_sleep_until)]
#![feature(oneshot_channel)]
#![feature(mpmc_channel)]
#![allow(internal_features)]
#![reexport_test_harness_main = "harness_main"]
extern crate oxbow_rt;

// Real std test files, verbatim from rust's library/std/tests/.
#[cfg(test)] #[path = "t_num.rs"]     mod t_num;
#[cfg(test)] #[path = "t_once.rs"]    mod t_once;
#[cfg(test)] #[path = "t_oneshot.rs"] mod t_oneshot;
#[cfg(test)] #[path = "t_barrier.rs"] mod t_barrier;
#[cfg(test)] #[path = "t_thread.rs"]  mod t_thread;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
