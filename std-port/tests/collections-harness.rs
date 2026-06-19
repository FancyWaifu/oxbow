#![no_main]
#![feature(custom_test_frameworks)]
#![feature(try_reserve_kind)]
#![feature(assert_matches)]
#![feature(hash_extract_if)]
#![feature(const_trait_impl)]
#![feature(const_default)]
#![allow(internal_features)]
#![reexport_test_harness_main = "harness_main"]
extern crate oxbow_rt;
extern crate std as realstd; // the test file refers to `realstd::`

// Re-export std items so the test file's `crate::X` paths resolve to std.
pub use std::{boxed, cell, cmp, fmt, hash, iter, panic, rc, string, sync, vec};
pub use std::assert_matches;

#[cfg(test)] mod testing;
mod btreeset {
    pub use std::collections::BTreeSet;
    pub use std::collections::btree_set::*;
    pub use std::cmp::Ordering;
    pub use std::hash::{Hash, Hasher};
    pub use std::fmt::Debug;
    #[cfg(test)] mod tests;
}

pub mod test_helpers {
    // A deterministic, entropy-free RNG for the tests (fixed seed).
    pub fn test_rng() -> rand_xorshift::XorShiftRng {
        rand::SeedableRng::from_seed([0x42u8; 16])
    }
}

// Wrap the verbatim std HashMap test file so its `super::HashMap`/`super::Entry` resolve.
mod hashmap {
    pub use std::collections::HashMap;
    pub use std::collections::hash_map::Entry;
    #[cfg(test)] mod tests;
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! { harness_main(); std::process::exit(0); }
