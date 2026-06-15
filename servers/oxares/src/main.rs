//! cares-test — the c-ares DNS port. Stage A proved the vendored library
//! compiles + links; Stage B bridges its async socket model onto oxbow's net
//! server (see `oxudp.rs` for the extern "C" UDP helpers and `cares_glue.c` for
//! the socket-function callbacks + the synchronous driving loop). The C `main`
//! resolves a hostname through real c-ares and prints the address.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;

mod oxudp;
