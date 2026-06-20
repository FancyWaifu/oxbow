// oxbow-native symlink/hardlink/canonicalize tests (sys/fs/oxbow.rs + fsd TAG_FS_SYMLINK/
// READLINK/LINK + kind-aware OPEN). Uses the public std::fs API.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features, deprecated)]
extern crate oxbow_rt;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

#[test]
fn symlink_readlink_roundtrip() {
    let target = "/symt_target.txt";
    let link = "/symt_link";
    File::create(target).unwrap().write_all(b"hello").unwrap();
    let _ = fs::remove_file(link);
    fs::soft_link(target, link).unwrap();
    assert_eq!(fs::read_link(link).unwrap(), PathBuf::from(target));
    fs::remove_file(link).ok();
    fs::remove_file(target).ok();
}

#[test]
fn lstat_vs_stat_follows() {
    let target = "/symt2_target.txt";
    let link = "/symt2_link";
    File::create(target).unwrap().write_all(b"abc").unwrap();
    let _ = fs::remove_file(link);
    fs::soft_link(target, link).unwrap();
    // symlink_metadata = lstat: the link itself
    let l = fs::symlink_metadata(link).unwrap();
    assert!(l.file_type().is_symlink());
    assert!(!l.file_type().is_file());
    // metadata = stat: follows to the target (a regular file)
    let md = fs::metadata(link).unwrap();
    assert!(md.is_file());
    assert!(!md.file_type().is_symlink());
    assert_eq!(md.len(), 3); // "abc"
    fs::remove_file(link).ok();
    fs::remove_file(target).ok();
}

#[test]
fn broken_symlink() {
    let link = "/symt3_link";
    let _ = fs::remove_file(link);
    fs::soft_link("/does-not-exist-xyz", link).unwrap(); // a symlink may dangle
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert!(fs::metadata(link).is_err()); // follow -> target missing
    fs::remove_file(link).ok();
}

#[test]
fn hard_link_shares_content() {
    let a = "/symt4_a.txt";
    let b = "/symt4_b.txt";
    File::create(a).unwrap().write_all(b"shared").unwrap();
    let _ = fs::remove_file(b);
    fs::hard_link(a, b).unwrap();
    let mut s = String::new();
    File::open(b).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "shared");
    assert!(fs::metadata(b).unwrap().is_file());
    fs::remove_file(a).ok();
    fs::remove_file(b).ok();
}

#[test]
fn canonicalize_resolves_symlink() {
    let target = "/symt5_target.txt";
    let link = "/symt5_link";
    File::create(target).unwrap();
    let _ = fs::remove_file(link);
    fs::soft_link(target, link).unwrap();
    assert_eq!(fs::canonicalize(link).unwrap(), PathBuf::from(target));
    assert_eq!(fs::canonicalize("/./symt5_link").unwrap(), PathBuf::from(target));
    fs::remove_file(link).ok();
    fs::remove_file(target).ok();
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
