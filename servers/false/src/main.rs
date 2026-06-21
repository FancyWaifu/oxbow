//! `false` — a verbatim sbase tool on oxbow. The runtime + libc live in oxbow-libc;
//! this crate just links the C (compiled by build.rs), which provides `main`.
#![no_std]
#![no_main]

extern crate oxbow_libc as _;
