// process/Command suite for oxbow. Two parts:
//  (A) VERBATIM-portable tests from library/std/src/process/tests.rs (no real child):
//      Send+Sync, fail-to-start NotFound, interior-NUL -> InvalidInput.
//  (B) oxbow-NATIVE backend tests: std::process::Command spawning a real /bin program
//      (/bin/hello prints to stdout + exits 0), exercising spawn/status/output(capture)/
//      try_wait/wait against the oxbow process backend (sys/process/oxbow.rs).
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::io::ErrorKind;
use std::process::Command;
use std::time::Duration;

// A "known command" that exists on oxbow (std's suite uses `echo`; oxbow has /bin/hello).
fn known_command() -> Command {
    Command::new("/bin/hello")
}

// ---------- (A) verbatim-portable std tests ----------

#[test]
fn test_command_implements_send_sync() {
    fn take_send_sync_type<T: Send + Sync>(_: T) {}
    take_send_sync_type(Command::new(""))
}

#[test]
fn test_process_output_fail_to_start() {
    match Command::new("/no-binary-by-this-name-should-exist").output() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::NotFound),
        Ok(..) => panic!(),
    }
}

#[test]
fn test_interior_nul_in_progname_is_error() {
    match Command::new("has-some-\0\0s-inside").spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput),
        Ok(_) => panic!(),
    }
}

#[test]
fn test_interior_nul_in_arg_is_error() {
    match known_command().arg("has-some-\0\0s-inside").spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput),
        Ok(_) => panic!(),
    }
}

#[test]
fn test_interior_nul_in_args_is_error() {
    match known_command().args(&["has-some-\0\0s-inside"]).spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput),
        Ok(_) => panic!(),
    }
}

#[test]
fn test_interior_nul_in_current_dir_is_error() {
    match known_command().current_dir("has-some-\0\0s-inside").spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput),
        Ok(_) => panic!(),
    }
}

#[test]
fn test_interior_nul_in_env_key_is_error() {
    match known_command().env("has-some-\0\0s-inside", "value").spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput),
        Ok(_) => panic!(),
    }
}

#[test]
fn test_interior_nul_in_env_value_is_error() {
    match known_command().env("key", "has-some-\0\0s-inside").spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput),
        Ok(_) => panic!(),
    }
}

// ---------- (B) oxbow-native backend tests ----------

#[test]
fn native_spawn_status_success() {
    let status = Command::new("/bin/hello").status().expect("spawn /bin/hello");
    assert!(status.success(), "status = {status:?}");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn native_output_captures_stdout() {
    let out = Command::new("/bin/hello").output().expect("output /bin/hello");
    assert!(out.status.success(), "status = {:?}", out.status);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("hello"), "captured stdout was {s:?}");
}

#[test]
fn native_spawn_nonexistent_is_notfound() {
    match Command::new("/bin/definitely-not-here").spawn() {
        Err(e) => assert_eq!(e.kind(), ErrorKind::NotFound),
        Ok(_) => panic!(),
    }
}

#[test]
fn native_try_wait_then_wait() {
    let mut child = Command::new("/bin/hello").spawn().expect("spawn");
    let mut observed_exit = false;
    for _ in 0..200 {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success());
                observed_exit = true;
                break;
            }
            None => std::thread::sleep(Duration::from_millis(15)),
        }
    }
    assert!(observed_exit, "child never reported exit via try_wait");
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
