//! cares-test — exercises libc's getaddrinfo, now backed by c-ares system-wide.
//! The real work is in `oxmain.c`; this just provides the no_std entry shim.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
