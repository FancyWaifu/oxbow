//! tcc on oxbow — TinyCC compiled for oxbow against oxbow-libc. The C `main`
//! (tcc.c) is the entry, via oxbow-libc's oxbow_main. (Running it fully needs
//! the JIT exec capability — Phase C.)
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
