# Rust `std` port for oxbow (`x86_64-unknown-oxbow`)

Goal: cross-compile real Rust `std` programs (on the host) that run on oxbow — the
foundation for everything up to (eventually) on-device `rustc`. Architecture is a
**native `sys/pal/oxbow` backend** (the Redox model), NOT a Unix masquerade —
oxbow is spawn-not-fork, capability-based, no signals, so pretending to be Unix
would be permanent impedance mismatch.

## The target

`x86_64-unknown-oxbow.json` (repo root): x86_64 SysV ELF, hardware SSE
(`+sse,+sse2`), static relocation (no PIC GOT — matches the servers/libc), no
kernel code-model (userland is lower-half), `os = "oxbow"`, panic=abort.

Build for it with nightly + build-std:

```
cargo +nightly build --target x86_64-unknown-oxbow.json \
  -Z json-target-spec -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem
```

## Phases

- **Phase 0 — target + core/alloc.** ✅ DONE. The target spec validates; `core`,
  `compiler_builtins`, `alloc` and a `no_std`+`alloc` test crate build for it.
- **Phase 1 — minimal `std`.** ✅ DONE — **real Rust `std` RUNS on oxbow.** A
  cross-compiled `std` program (`Vec`, iterators, `String`, `println!`) runs as an
  oxbow process and prints to the console:
  ```
  hello from REAL Rust std on oxbow!
    squares = [1, 4, 9, 16, 25, 36]
    sum     = 91
    heap    = greetings via std::string::String
  ```
  Added `os = "oxbow"` support to rust's `library/std/src/sys` (a fork at the pinned
  nightly commit; patch + backend mirrored in `std-port/`): a System allocator,
  `getentropy` randomness, errno/ErrorKind mapping, stdio, and TLS routed to the
  single-threaded no-op path. **Key architecture decision:** rather than link
  oxbow-libc (a self-contained no_std staticlib that owns the panic handler +
  global allocator — an irreconcilable clash with std), the std backend is fully
  self-contained and calls thin C-ABI shims (`__oxbow_alloc`/`_write`/`_getentropy`/
  `_exit`) exported by **oxbow-rt under a new `hosted` feature**, which reuses its
  existing slab + syscall stubs and drops its own lang items when hosted. The
  program is `#![no_main]` + `#![feature(restricted_std)]`, provides a C `main` or
  `oxbow_main`, and links `oxbow-rt` (hosted) for `_start`. A size-optimised release
  build (`opt-level=z` + LTO + `optimize_for_size`) is **19 KB**.
- **Phase 2 — keystones.** In-process threads (`SYS_THREAD_SPAWN` sharing the
  current pml4 + a fresh stack; the SMP/TCB infra already exists), a futex
  (wait-on-address, from `notif` or a new syscall), a real wall clock (CMOS RTC →
  `SYS_WALLTIME`), and a real env block (passed at spawn like `SPAWN_ARGV`).
- **Phase 3 — harden.** Native ELF TLS, `Command` stdio piping (spawn-not-fork),
  full `Metadata`, optional `panic=unwind`.
- **Phase 4 — the std test suite** as the "done" bar.

## What oxbow already provides (so the green rows are mostly plumbing)

| std `sys` module | oxbow primitive |
|---|---|
| alloc | libc slab `malloc`/`free` |
| stdio | tty + `SYS_CONSOLE_WRITE` |
| fs | fsd: open/read/write/seek/create/unlink/rename/readdir |
| net | smoltcp TCP, UDP cap API, c-ares DNS |
| time (Instant) | `SYS_UPTIME_MS` |
| rand | `SYS_GETENTROPY` |
| process | `SYS_SPAWN`/`SYS_SPAWN_BYTES` + exit notif + pipes |

Keystone gaps (Phase 2): **in-process threads** (today = one thread per process),
**futex** (have `notif` wait/signal), **wall clock** (`gettimeofday` is uptime-
based, no epoch), **env** (`getenv` is a stub).

## Phase 1 reality: patching the Rust std source

`std`'s platform backend is selected by `#[cfg(target_os = ...)]` inside
rust-lang/rust's `library/std/`. There is no out-of-tree plugin point, so adding
`os = "oxbow"` means patching std source:

1. Add an `oxbow` arm to `library/std/src/sys/pal/mod.rs` (and any other cfg
   dispatch points).
2. Create `library/std/src/sys/pal/oxbow/` implementing the modules.
3. Build with `-Z build-std=std` so it compiles the patched std.

Two ways to host the patch during bring-up:
- **(a) Edit the toolchain's rust-src in place** (`~/.rustup/toolchains/<nightly>/
  lib/rustlib/src/rust/library/`) — fast to iterate, but `rustup update` wipes it.
  Good for proving the backend.
- **(b) A maintained rust fork / vendored checkout** + a linked toolchain — the
  production path (what Redox does). Heavier (the rust repo is ~1 GB) but durable.

Plan: prove it with (a), then migrate to (b). Keep the oxbow backend source under
version control in this repo and patch it into rust-src via a build script.
