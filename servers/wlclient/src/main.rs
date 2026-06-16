//! wlclient — a standalone Wayland client program, spawned by the oxcomp
//! compositor with one end of a channel as its Wayland socket (the inherited-fd
//! model). Proves real cross-process Wayland on oxbow. The work is in
//! wlclient.c; this is the no_std/libc entry shim.
#![no_std]
#![no_main]
extern crate oxbow_libc as _;
