// Adapted from rust-lang/rust library/std/tests/path.rs for the oxbow libtest harness.
// oxbow's std::path backend is the `_ => mod unix` default arm (unix-style `/` paths),
// but the tests gate behavior on cfg!(unix) (false for oxbow) -> added target_os="oxbow"
// to every unix cfg gate so the (correct) unix-path assertions run on oxbow.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![feature(clone_to_uninit)]
#![feature(normalize_lexically)]
#![feature(path_trailing_sep)]
#![allow(internal_features)]
extern crate oxbow_rt;

mod patht;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
