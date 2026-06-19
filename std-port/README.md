# oxbow Rust `std` backend (`x86_64-unknown-oxbow`)

Phase 1 (see `../docs/rust-std-port.md`). **Status: real Rust `std` RUNS on oxbow.**
A cross-compiled `std` program (`Vec`, iterators, `String`, `println!`) runs as an
oxbow process and prints to the console. A size-optimised release build is **19 KB**.

## Architecture (why no oxbow-libc)

oxbow-libc is a self-contained `no_std` staticlib that owns the panic handler +
global allocator — an irreconcilable clash with std (which owns both). So the std
backend is **self-contained** and calls thin C-ABI shims exported by **oxbow-rt
under its `hosted` feature** (`__oxbow_alloc`/`_alloc_zeroed`/`_dealloc`/`_write`/
`_read`/`_getentropy`/`_exit`), reusing oxbow-rt's slab + syscall stubs. With
`hosted`, oxbow-rt drops its own `#[global_allocator]` + `#[panic_handler]` so std
supplies them. A std program is `#![no_main]` + `#![feature(restricted_std)]`,
provides `oxbow_main` (or a C `main`), and depends on `oxbow-rt` with
`features = ["hosted"]` for `_start`.

## What's here

- `oxbow-std-backend.patch` — the patch against rust-lang/rust's `library/std/src/sys`
  (nightly commit `4b0c9d76a`) adding `os = "oxbow"`: System allocator, `getentropy`
  randomness, errno/ErrorKind mapping, stdio (console), TLS → single-threaded
  no-op-guard path. 9 files, ~137 insertions.
- `sys-oxbow/` — readable copies of the four new backend modules (alloc, random,
  io/error, stdio). The patch is the source of truth for applying.

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
