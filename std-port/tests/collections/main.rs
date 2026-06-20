// Adapted from rust-lang/rust library/std/src/collections/hash/{map,set}/tests.rs for the
// oxbow libtest harness. Exercises HashMap/HashSet (SwissTable) + RandomState entropy
// (SYS_GETENTROPY via oxbow-rt). realstd::->std::, super::->std::collections::, crate::->std::.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![feature(assert_matches)]
#![feature(try_reserve_kind)]
#![feature(hash_extract_if)]
#![allow(internal_features)]
extern crate oxbow_rt;

mod mapt;
mod sett;

// Stand-in for std's internal crate::test_helpers::test_rng (not reachable externally).
pub(crate) fn test_rng() -> rand_xorshift::XorShiftRng {
    use core::hash::{BuildHasher, Hash, Hasher};
    let mut hasher = std::hash::RandomState::new().build_hasher();
    core::panic::Location::caller().hash(&mut hasher);
    let hc64 = hasher.finish();
    let seed_vec = hc64.to_le_bytes().into_iter().chain(0u8..8).collect::<Vec<u8>>();
    let seed: [u8; 16] = seed_vec.as_slice().try_into().unwrap();
    rand::SeedableRng::from_seed(seed)
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
