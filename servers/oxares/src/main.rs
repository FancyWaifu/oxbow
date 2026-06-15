//! cares-test — Stage A of the c-ares port: prove the vendored c-ares library
//! compiles + links on oxbow. The C main prints the c-ares version.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
