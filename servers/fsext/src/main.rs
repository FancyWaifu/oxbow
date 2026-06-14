//! fstest — Stage-1 proof that lwext4 (ext2) builds and runs on oxbow. The C
//! `main` (src/oxmain.c) creates a RAM-backed block device, mkfs's an ext2
//! filesystem on it, mounts it, and does real file + directory I/O. Entry is
//! oxbow-libc's oxbow_main, which calls the C main().
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
