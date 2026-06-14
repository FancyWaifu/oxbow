//! Lua on oxbow — the Lua 5.4 interpreter compiled for oxbow against oxbow-libc.
//! The C `main` (src/oxmain.c) is the entry, via oxbow-libc's oxbow_main: it
//! creates a Lua state, opens the libraries that work on oxbow, and runs a
//! script (a built-in test, or a .lua file named in argv).
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
