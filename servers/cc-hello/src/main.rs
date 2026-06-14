//! cc-hello — a C program on oxbow. All the runtime + libc lives in oxbow-libc;
//! this crate just supplies `src/hello.c` (compiled by build.rs) and links the
//! libc, which provides `_start`/`oxbow_main` → the C `main`.
#![no_std]
#![no_main]

extern crate oxbow_libc as _;
