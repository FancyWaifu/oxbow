#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![allow(internal_features)]
extern crate oxbow_rt;

// Re-export std modules so the test bodies' `crate::X` paths resolve to std.
pub use std::{fs, io, mem, path, str};

// oxbow-flavored test_helpers: tmpdir() builds a unique dir under env::temp_dir()
// (= /tmp on oxbow), ensuring the base exists; TempDir cleans up via remove_dir_all.
#[cfg(test)]
pub mod test_helpers {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    pub struct TempDir(PathBuf);
    impl TempDir {
        pub fn join(&self, p: &str) -> PathBuf {
            self.0.join(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    pub fn test_rng() -> rand_xorshift::XorShiftRng {
        rand::SeedableRng::from_seed([0x33u8; 16])
    }
    static N: AtomicU32 = AtomicU32::new(0);
    pub fn tmpdir() -> TempDir {
        let mut base = std::env::temp_dir();
        let _ = std::fs::create_dir_all(&base);
        let n = N.fetch_add(1, Ordering::Relaxed);
        base.push(format!("rust-fs-{}", n));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).unwrap();
        TempDir(base)
    }
}

#[cfg(test)]
mod fstests;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
