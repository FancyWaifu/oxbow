# oxbow Rust `std` backend (`x86_64-unknown-oxbow`)

Phase 1 (see `../docs/rust-std-port.md`). **Status: `std` COMPILES for oxbow.**
A cross-compiled `std` hello-world builds into a 6 MB ELF for `x86_64-unknown-oxbow`.
Still TODO to *run* it: a real `sys/pal/oxbow` backend (stdout/exit/args/time) +
the entry-point glue (`_start` → `lang_start` → `main`).

## What's here

- `oxbow-std-backend.patch` — the patch against rust-lang/rust's `library/std/src/sys`
  (at the nightly commit `4b0c9d76a`) that adds `os = "oxbow"` support: a System
  allocator, `getentropy` randomness, errno/ErrorKind mapping, and routes TLS to
  the single-threaded `no_threads`/no-op-guard path. 7 files, ~121 insertions.
- `sys-oxbow/` — readable copies of the three new backend modules (the patch is the
  source of truth for applying).

## The fork setup (durable, your machine)

A shallow clone of rust-lang/rust at the toolchain's commit lives at `~/rust-oxbow`,
and the nightly toolchain's rust-src symlinks to it so `build-std` compiles the
patched std:

```
# one-time: clone the matching commit (no submodules needed for build-std except these)
git -c protocol.version=2 fetch --depth 1 origin 4b0c9d76ae7d387229caea55cfa73c280b08b8a7
git submodule update --init --depth 1 library/backtrace library/stdarch library/portable-simd
# point build-std at the fork (re-run after `rustup update` wipes rust-src):
ln -s ~/rust-oxbow "$(rustc +nightly --print sysroot)/lib/rustlib/src/rust"
# apply the oxbow backend:
git -C ~/rust-oxbow apply ~/oxbow/std-port/oxbow-std-backend.patch
```

## Building a std program for oxbow

```
cargo +nightly build --target x86_64-unknown-oxbow.json \
  -Z json-target-spec -Z build-std=std,panic_abort \
  -Z build-std-features=compiler-builtins-mem
```

The program must opt in with `#![feature(restricted_std)]` until the `pal/oxbow`
backend replaces the `unsupported` stubs (which is what makes std "known/supported").
