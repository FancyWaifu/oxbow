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
  path concepts with no cap-based-cwd equivalent — **now settled + implemented**, see
  the "env cwd/exe stance" section below.

## Collections: HashMap + BTreeSet — 85/85 pass (no source changes)

Inline std/alloc collection tests (`hash/map/tests.rs`, `btree/set/tests.rs`) are
coupled to internals via `crate::`/`super::`/`realstd::` + use `rand`. Run them with
`collections-harness.rs` scaffolding (no oxbow source changes):
- deps: `rand 0.8` + `rand_xorshift 0.3` (`default-features = false`); provide a
  fixed-seed `test_rng()` in a `test_helpers` module.
- `extern crate std as realstd;` (for `realstd::`).
- re-export std modules at the crate root (`pub use std::{cell,cmp,fmt,hash,...};`)
  so the test files' `crate::X` resolve.
- wrap each test file in a module that re-exports its type + needed traits, so
  `super::HashMap` / `super::*` resolve (e.g. `mod hashmap { pub use
  std::collections::HashMap; pub use std::collections::hash_map::Entry; mod tests; }`).
- copy the `alloctests/testing/` helpers for the btree tests (CrashTestDummy, rng).

BTreeMap's *own* tests poke private node internals (NodeRef/MIN_LEN/crate::testing),
so they aren't standalone-extractable; BTreeSet wraps BTreeMap and validates the tree.

## env cwd/exe stance — SETTLED + verified (`env-stance-test.rs`)

The three `std::env` path APIs that were "capability-model gaps" are now settled in
`~/rust-oxbow/.../sys/paths/oxbow.rs` + `rt::__oxbow_chdir`. Principle: **the cwd is the
slot-1 spawn-root capability; the path is relative to it** (`/` = slot 1; you navigate
within your subtree, never above it — fsd rejects `..` as L3 confinement).

- `current_dir()` → process-local path *label* (default `/`); informational, no authority.
- `set_current_dir(p)` → std folds `.`/`..` lexically to an absolute target, then
  `__oxbow_chdir` opens it **from the root cap (slot 1)** and installs the dir cap as the
  cwd, so relative fs ops + child spawns follow it. Re-opening from root (not relatively)
  makes descent/ascent/multi-component uniform without fsd `..` support.
- `current_exe()` → `Err(Unsupported)` — oxbow spawns from bytes, there is no exe path.

`env-stance-test.rs` is a plain (non-libtest) `oxbow_main` program: build with
`-Z build-std=std,panic_unwind -Z build-std-features=compiler-builtins-mem` (set
`RUST_TARGET_PATH=~/oxbow` + `RUSTFLAGS=-Zunstable-options` for the custom-target probe),
inject to `/bin/oxtest`, run via `env-stance-harness.py`. All checks pass: defaults to
`/`, chdir descent + `..` ascent, relative writes land under the new cwd (absent from
`/`), qualified multi-component reads resolve, bad chdir errors + leaves cwd unchanged.
The only real-std `env.rs` test left red is `test_self_exe_path` (asserts
`current_exe().is_ok()`) — the expected cost of the honest `Err`.
