// Adapted from rust-lang/rust library/std/src/io/{cursor,util,buffered}/tests.rs for the
// oxbow libtest harness. In-memory io machinery: Cursor (read/write/seek/vectored),
// Empty/Repeat/Sink, BufReader/BufWriter/LineWriter. crate:: -> std::; #[bench] fns stripped.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![feature(read_buf)]
#![feature(core_io_borrowed_buf)]
#![feature(borrowed_buf_init)]
#![feature(can_vector)]
#![feature(write_all_vectored)]
#![feature(io_const_error)]
#![feature(buf_read_has_data_left)]
#![feature(seek_stream_len)]
#![feature(seek_io_take_position)]
#![feature(io_slice_as_bytes)]
#![feature(cursor_split)]
#![allow(internal_features)]
extern crate oxbow_rt;

mod cursor;
mod util;
mod buffered;
mod iotop;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
