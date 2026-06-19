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
