#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![feature(const_default)]
#![feature(const_trait_impl)]
#![feature(mapped_lock_guards)]
#![feature(once_cell_try)]
#![feature(lock_value_accessors)]
#![feature(reentrant_lock)]
#![feature(std_internals)]
#![feature(sync_nonpoison)]
#![feature(sync_poison_mod)]
#![feature(nonpoison_condvar)]
#![feature(nonpoison_mutex)]
#![feature(nonpoison_rwlock)]
#![allow(internal_features)]
#![feature(macro_metavar_expr_concat)]
extern crate oxbow_rt;

mod barrier;
mod condvar;
mod lazy_lock;
mod mutex;
mod once_lock;
mod reentrant_lock;
mod rwlock;
#[path = "common/mod.rs"]
mod common;

#[track_caller]
fn result_unwrap<T, E: std::fmt::Debug>(x: Result<T, E>) -> T { x.unwrap() }

macro_rules! nonpoison_and_poison_unwrap_test {
    ( name: $name:ident, test_body: {$($test_body:tt)*} ) => {
        #[test]
        fn ${concat(nonpoison_, $name)}() {
            #[allow(unused_imports)]
            use ::std::convert::identity as maybe_unwrap;
            use ::std::sync::nonpoison as locks;
            $($test_body)*
        }
        #[test]
        fn ${concat(poison_, $name)}() {
            #[allow(unused_imports)]
            use super::result_unwrap as maybe_unwrap;
            use ::std::sync::poison as locks;
            $($test_body)*
        }
    }
}
use nonpoison_and_poison_unwrap_test;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! { harness_main(); std::process::exit(0); }
