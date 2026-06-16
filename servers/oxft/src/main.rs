//! ft-test — prove the vendored FreeType initialises + rasterizes a glyph on
//! oxbow (§51). The real work is in oxmain.c; this is the no_std shim.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
