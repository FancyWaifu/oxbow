// Confirms current_exe() returns a specific Unsupported error (not a panic / wrong kind).
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::env;
use std::io::ErrorKind;

#[test]
fn current_exe_is_sensible_unsupported() {
    let e = env::current_exe().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    let msg = e.to_string();
    // a self-explanatory message, not the generic "operation not supported on this platform"
    assert!(msg.contains("oxbow"), "message was: {msg}");
    assert!(msg.contains("ELF") || msg.contains("executable path"), "message was: {msg}");
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
