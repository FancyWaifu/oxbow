//! QuickJS on oxbow — the QuickJS-ng JavaScript engine compiled for oxbow
//! against oxbow-libc. The C `main` (src/oxmain.c) is the entry, via
//! oxbow-libc's oxbow_main: it creates a JS runtime+context, installs `print`/
//! `console.log`, and evaluates a script (a built-in test, or a .js file).
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
