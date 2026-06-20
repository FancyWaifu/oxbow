// oxbow-native File::try_clone tests. The clone shares the same FileInner (Arc):
// one fd, one cursor, one cached size (POSIX dup), closed on last drop.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::fs::{self, File};
use std::io::{Read, Write};

#[test]
fn try_clone_reads_same_file() {
    let p = "/clone_read.txt";
    File::create(p).unwrap().write_all(b"clonedata").unwrap();
    let f = File::open(p).unwrap();
    let mut g = f.try_clone().unwrap();
    let mut buf = [0u8; 4];
    g.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"clon");
    fs::remove_file(p).ok();
}

#[test]
fn try_clone_shares_cursor() {
    // POSIX dup semantics: a read through either handle advances the SHARED cursor.
    let p = "/clone_cursor.txt";
    File::create(p).unwrap().write_all(b"0123456789").unwrap();
    let mut f = File::open(p).unwrap();
    let mut g = f.try_clone().unwrap();
    let mut b1 = [0u8; 3];
    f.read_exact(&mut b1).unwrap(); // "012", cursor -> 3
    assert_eq!(&b1, b"012");
    let mut b2 = [0u8; 3];
    g.read_exact(&mut b2).unwrap(); // "345" because the cursor is shared, not reset
    assert_eq!(&b2, b"345");
    fs::remove_file(p).ok();
}

#[test]
fn try_clone_refcounts_close() {
    // Dropping one clone must NOT close the fd out from under the other.
    let p = "/clone_drop.txt";
    File::create(p).unwrap().write_all(b"persist").unwrap();
    let f = File::open(p).unwrap();
    let mut g = f.try_clone().unwrap();
    drop(f); // one handle closed; the shared fd must survive
    let mut s = String::new();
    g.read_to_string(&mut s).unwrap();
    assert_eq!(s, "persist");
    fs::remove_file(p).ok();
}

#[test]
fn try_clone_write_visible() {
    let p = "/clone_write.txt";
    let f = File::create(p).unwrap();
    let mut g = f.try_clone().unwrap();
    g.write_all(b"viaClone").unwrap();
    drop(g);
    drop(f);
    assert_eq!(fs::read_to_string(p).unwrap(), "viaClone");
    fs::remove_file(p).ok();
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
