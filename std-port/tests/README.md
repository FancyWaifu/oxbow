# Running real Rust std tests on oxbow (Phase 4)

`harness.rs` is the test-runner skeleton: `#![no_main]` + libtest's harness reexported
as `harness_main` and called from `oxbow_main`. Drop real std test files from
`~/rust-oxbow/library/std/tests/` into the crate as modules (`#[path=...] mod t_x;`),
then build with the **libtest** harness (panic=unwind is required — libtest isolates
failing/`should_panic` tests via `catch_unwind`):

```
cargo +nightly build --release \
  --target x86_64-unknown-oxbow.json -Z json-target-spec \
  -Z build-std=std,test,panic_unwind -Z build-std-features=compiler-builtins-mem --tests
```

The test binary is `target/x86_64-unknown-oxbow/release/deps/<crate>-<hash>`; inject it
into the disk and run it from the shell. Crate-level `#![feature(...)]` from a std test
file (and from `sync/lib.rs`) must move to the harness crate root.

Status (2026-06-19): 56/57 pass across `thread.rs`, `num.rs`, `sync/{once,oneshot,barrier}.rs`.
Heavy `mpsc::*_stress` tests hang under the busy-yield scheduler (follow-up).

## Broadening (2026-06-19): env / fs / collections — 64/67 pass

- collections: `alloctests/tests/string.rs` (60 tests) — needs crate-root features
  `try_reserve_kind`, `string_from_utf8_lossy_owned`, `string_remove_matches`,
  `string_replace_in_place`, `try_with_capacity` (no rand).
- env: `std/tests/env.rs` passes. `env_modify.rs` + the `mod common;` helper pull
  `rand` (for `test_rng`) — provide a minimal rand-free `common.rs` (`TempDir`+`tmpdir()`).
- Fixed an allocator self-deadlock: `try_reserve(isize::MAX)` indexed `free[63]`
  out-of-bounds while holding the heap spinlock → re-entrant alloc self-deadlock.
  Fix in `rt`: fail fast when the size class exceeds the slab.
- Remaining failures = `env::current_dir`/`current_exe`/`set_current_dir`: POSIX
  path concepts with no cap-based-cwd equivalent (a design decision).
