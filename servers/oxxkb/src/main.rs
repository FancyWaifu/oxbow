//! xkb-test — prove the vendored libxkbcommon compiles a keymap and decodes
//! keycodes → characters on oxbow (§48). The real work is in oxmain.c; this is
//! the no_std entry shim (libc's `entry` feature provides _start → main).
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
