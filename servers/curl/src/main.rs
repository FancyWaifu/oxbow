//! curl on oxbow — libcurl (HTTP-only, no TLS) compiled for oxbow against
//! oxbow-libc + its BSD-sockets shim. The C `main` (src/oxmain.c) fetches a URL.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
