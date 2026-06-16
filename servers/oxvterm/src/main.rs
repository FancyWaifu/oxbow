//! vterm-test — prove the vendored libvterm parses terminal output into a screen
//! grid on oxbow (§50). The real work is in oxmain.c; this is the no_std shim.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
