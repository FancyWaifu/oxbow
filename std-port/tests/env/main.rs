// Adapted from rust-lang/rust library/std/tests/{env,env_modify}.rs for the oxbow libtest
// harness. env var get/set/remove (oxbow's in-process env table), vars()/args() Debug.
// Excluded: test_self_exe_path (current_exe unsupported on oxbow), split_paths/join_paths
// (#[cfg(unix)] + oxbow routes them to `unsupported`), env_home_dir (auto-ignored: not unix/windows).
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

mod common;
mod envt;
mod envmodt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! { harness_main(); std::process::exit(0); }
