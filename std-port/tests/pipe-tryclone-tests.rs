// oxbow-native pipe try_clone tests via std::io::pipe (PipeReader/PipeWriter).
// try_clone -> SYS_CAP_DUP: a fresh kernel handle to the same pipe object.
// NOTE: oxbow pipe EOF is explicit (close doesn't refcount-EOF), so these tests
// read EXACT byte counts rather than read-to-EOF.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::io::{pipe, Read, Write};

#[test]
fn clone_writer_both_write() {
    let (mut r, mut w) = pipe().unwrap();
    let mut w2 = w.try_clone().unwrap();
    w.write_all(b"AA").unwrap();
    w2.write_all(b"BB").unwrap();
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"AABB");
}

#[test]
fn clone_reader_reads_shared_stream() {
    let (mut r, mut w) = pipe().unwrap();
    let mut r2 = r.try_clone().unwrap();
    w.write_all(b"0123").unwrap();
    let mut a = [0u8; 2];
    r.read_exact(&mut a).unwrap(); // "01"
    assert_eq!(&a, b"01");
    let mut b = [0u8; 2];
    r2.read_exact(&mut b).unwrap(); // "23" from the SAME pipe (shared stream)
    assert_eq!(&b, b"23");
}

#[test]
fn clone_writer_survives_drop_of_original() {
    let (mut r, w) = pipe().unwrap();
    let mut w2 = w.try_clone().unwrap();
    w2.write_all(b"Z").unwrap();
    drop(w); // closing one handle must NOT destroy the pipe under w2
    w2.write_all(b"W").unwrap();
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ZW");
}

#[test]
fn clone_reader_survives_drop_of_original() {
    let (r, mut w) = pipe().unwrap();
    let mut r2 = r.try_clone().unwrap();
    drop(r); // one reader handle closed; the pipe survives for r2
    w.write_all(b"keep").unwrap();
    let mut buf = [0u8; 4];
    r2.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"keep");
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
