#![no_main]
extern crate oxbow_rt;

use std::env;
use std::fs;
use std::path::Path;

fn check(cond: bool, name: &str) {
    println!("{} - {}", if cond { "ok  " } else { "FAIL" }, name);
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    println!("== env stance test ==");

    // 1. current_dir() defaults to "/" (informational label; real cwd is the slot-1 cap)
    let cwd0 = env::current_dir();
    println!("current_dir() = {:?}", cwd0);
    check(cwd0.ok().as_deref() == Some(Path::new("/")), "current_dir defaults to /");

    // 2. current_exe() is Err — oxbow spawns from bytes, there is no exe path
    let exe = env::current_exe();
    println!("current_exe() = {:?}", exe);
    check(exe.is_err(), "current_exe is Err (no exe path on oxbow)");

    // 3. make a fresh dir under cwd and chdir into it (re-roots the cwd capability)
    let _ = fs::create_dir("envtest_dir");
    let r = env::set_current_dir("envtest_dir");
    println!("set_current_dir(envtest_dir) = {:?}", r);
    check(r.is_ok(), "set_current_dir into new dir ok");

    // 4. current_dir() reflects the new path
    let cwd1 = env::current_dir();
    println!("current_dir() = {:?}", cwd1);
    check(cwd1.ok().as_deref() == Some(Path::new("/envtest_dir")), "current_dir reflects chdir");

    // 5. a RELATIVE write now lands in the new dir — the cap genuinely followed
    fs::write("inside.txt", b"hello-oxbow").unwrap();
    let here = fs::read_to_string("inside.txt");
    println!("read inside.txt (relative) = {:?}", here);
    check(here.ok().as_deref() == Some("hello-oxbow"), "relative write/read follows new cwd");

    // 6. chdir back up; current_dir() returns to "/"
    let r2 = env::set_current_dir("..");
    println!("set_current_dir(..) = {:?}", r2);
    let cwd2 = env::current_dir();
    println!("current_dir() = {:?}", cwd2);
    check(cwd2.ok().as_deref() == Some(Path::new("/")), "chdir .. returns to /");

    // 7. the file is NOT a bare name at "/", but IS reachable via the qualified path —
    //    proves the relative write in step 5 went to envtest_dir, not "/"
    let bare = fs::read_to_string("inside.txt");
    println!("read /inside.txt = {:?}", bare);
    check(bare.is_err(), "inside.txt is not at / (was written under envtest_dir)");
    let qualified = fs::read_to_string("envtest_dir/inside.txt");
    println!("read envtest_dir/inside.txt = {:?}", qualified);
    check(qualified.ok().as_deref() == Some("hello-oxbow"), "file reachable via envtest_dir/inside.txt");

    // 8. set_current_dir to a missing path errors (and does not move the cwd)
    let bad = env::set_current_dir("no_such_dir_xyz");
    println!("set_current_dir(no_such_dir_xyz) = {:?}", bad);
    check(bad.is_err(), "set_current_dir on missing path errors");
    let cwd3 = env::current_dir();
    check(cwd3.ok().as_deref() == Some(Path::new("/")), "cwd unchanged after failed chdir");

    println!("== env stance test done ==");
    std::process::exit(0);
}
