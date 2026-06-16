//! oxterm — a Wayland terminal: it runs the shell, feeds its output through
//! libvterm, rasterizes the grid with FreeType, and shows it in a window (§52).
//! Keys arrive via wl_keyboard + xkb. This is the no_std entry shim.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
