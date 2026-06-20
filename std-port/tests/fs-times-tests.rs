// oxbow-native tests for the fs timestamps + truncate cluster (sys/fs/oxbow.rs +
// fsd TAG_FS_TRUNCATE/SETTIMES + extended TAG_FS_OPEN reply). Uses the public std::fs API.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Write};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn set_len_shrinks_and_grows() {
    let p = "/fstime_trunc.bin";
    let mut f = File::create(p).unwrap();
    f.write_all(&[7u8; 100]).unwrap();
    assert_eq!(fs::metadata(p).unwrap().len(), 100);

    // shrink
    f.set_len(10).unwrap();
    assert_eq!(fs::metadata(p).unwrap().len(), 10);
    let mut buf = Vec::new();
    File::open(p).unwrap().read_to_end(&mut buf).unwrap();
    assert_eq!(buf, vec![7u8; 10]);

    // grow (zero-extends)
    f.set_len(50).unwrap();
    assert_eq!(fs::metadata(p).unwrap().len(), 50);
    let mut buf2 = Vec::new();
    File::open(p).unwrap().read_to_end(&mut buf2).unwrap();
    assert_eq!(buf2.len(), 50);
    assert_eq!(&buf2[..10], &[7u8; 10]);
    assert_eq!(&buf2[10..], &[0u8; 40]); // grown region is zero

    fs::remove_file(p).ok();
}

#[test]
fn set_times_roundtrip() {
    let p = "/fstime_times.bin";
    File::create(p).unwrap().write_all(b"x").unwrap();

    let m = UNIX_EPOCH + Duration::from_secs(1_000_000_000); // 2001-09-09
    let a = UNIX_EPOCH + Duration::from_secs(1_500_000_000); // 2017-07-14
    let ft = FileTimes::new().set_modified(m).set_accessed(a);
    OpenOptions::new().write(true).open(p).unwrap().set_times(ft).unwrap();

    let md = fs::metadata(p).unwrap();
    assert_eq!(md.modified().unwrap(), m);
    assert_eq!(md.accessed().unwrap(), a);

    fs::remove_file(p).ok();
}

#[test]
fn set_only_modified_leaves_atime() {
    let p = "/fstime_partial.bin";
    File::create(p).unwrap();
    // seed both
    let base = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    OpenOptions::new()
        .write(true)
        .open(p)
        .unwrap()
        .set_times(FileTimes::new().set_modified(base).set_accessed(base))
        .unwrap();
    // now change only modified
    let newm = UNIX_EPOCH + Duration::from_secs(1_200_000_000);
    OpenOptions::new()
        .write(true)
        .open(p)
        .unwrap()
        .set_times(FileTimes::new().set_modified(newm))
        .unwrap();
    let md = fs::metadata(p).unwrap();
    assert_eq!(md.modified().unwrap(), newm);
    assert_eq!(md.accessed().unwrap(), base); // untouched
    fs::remove_file(p).ok();
}

#[test]
fn modified_and_accessed_readable() {
    let p = "/fstime_read.bin";
    File::create(p).unwrap();
    let md = fs::metadata(p).unwrap();
    assert!(md.modified().is_ok());
    assert!(md.accessed().is_ok());
    fs::remove_file(p).ok();
}

#[test]
fn created_is_unsupported() {
    let p = "/fstime_created.bin";
    File::create(p).unwrap();
    // ext2 has no birth time -> created() must be an error, not a bogus value.
    assert!(fs::metadata(p).unwrap().created().is_err());
    fs::remove_file(p).ok();
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
