//! MicroPython on oxbow — the MicroPython VM (minimal port) compiled for oxbow
//! against oxbow-libc. The C `main` (port/oxmain.c) is the entry, via
//! oxbow-libc's oxbow_main: it sets up a GC heap, mp_init, and runs a script
//! (a built-in test, or a .py file named in argv).
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
