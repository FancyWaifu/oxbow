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
- **Phase 2 — keystones.** 🟡 IN PROGRESS.
  - ✅ **Wall clock** — `SYS_WALLTIME` (52) reads the CMOS RTC (`kernel/.../rtc.rs`)
    → `(epoch_secs, nanos)`; oxbow-rt shims `__oxbow_walltime`/`__oxbow_uptime_ms`;
    std `sys/time/oxbow.rs` gives a real `SystemTime::now()` (verified: prints the
    correct UTC date) and a monotonic `Instant`.
  - ✅ **Thread + futex kernel foundation** — `SYS_THREAD_SPAWN` (53, a thread in
    the caller's address space via `spawn_user(current_proc, current_cr3, …)`),
    `SYS_THREAD_EXIT` (54, `exit_current` — does NOT kill the process),
    `SYS_FUTEX_WAIT`/`WAKE` (55/56, a per-process wait queue keyed on a user vaddr,
    reusing the kernel's block/wake + a `Tcb.futex_addr` field). oxbow-rt wrappers +
    a `spawn_thread`/`thread_trampoline` helper. Verified by `servers/thrtest`
    (`/bin/thrtest`): a worker thread increments a shared counter to 200000 and
    signals via the futex while main blocks, then thread-exits — parent survives.
  - ✅ **std `thread::spawn` + `Mutex`/`Condvar`/`RwLock`/`Once` + `join`** — verified:
    4 threads each sum 0..100000, accumulate into an `Arc<Mutex<u64>>`, and return
    their partial via `join()`; the Mutex total and the join-sum both equal
    19999800000. Pieces: (1) **thread-safe slab** (spinlock, committed d0bd245);
    (2) **futex backend** `sys/pal/unsupported/futex.rs` (oxbow-only) over
    `SYS_FUTEX_*`, wired into every `sync/*/mod.rs` futex arm; (3) **pal thread**
    `sys/thread/oxbow.rs` — `Thread::new` allocs a stack + spawns via
    `SYS_THREAD_SPAWN`, `join` futex-waits a join word the kernel sets on
    `SYS_THREAD_EXIT` *after* the thread is off its stack (so the stack frees
    safely); (4) **keyed per-thread TLS** `thread_local/key/oxbow.rs` — a
    `(tid, key)` table indexed by `SYS_THREAD_ID` (each thread owns its row →
    race-free; makes `CURRENT` per-thread, fixing the no_threads UAF). New syscalls
    `SYS_THREAD_ID`(57), `SYS_YIELD`(58). Limitations: no futex timeout yet
    (`Condvar::wait_timeout` blocks until woken); TLS destructors don't run (guard is
    a no-op → TLS values leak at thread exit); boot-module std demos need a bumped
    Memory budget (shell-funded children get plenty).
  - ✅ **env + args** — `std::env::args()` reads `SPAWN_ARGV` (the kernel now maps it
    for boot modules too, not just shell-spawned children) via the rt `__oxbow_argv`
    shim → `sys/args/oxbow.rs`. `std::env::var`/`set_var`/`vars`/`remove_var` work via
    an in-process table (`sys/env/oxbow.rs`, using the now-working `Mutex`), seeded
    with defaults (PATH=/bin, HOME=/home, TERM=oxterm) — oxbow has no spawn-passed
    env block yet, so vars are process-local + the defaults.

  **Phase 2 DONE.** A comprehensive demo (`std-port/oxhello-demo.elf`) prints the
  wall clock, env/args, and a 4-thread `Arc<Mutex<u64>>` sum — alloc, time, env,
  threads, Mutex, and TLS all in one std program.
- **Phase 3 — capabilities + harden.** 🟡 IN PROGRESS.
  - ✅ **`std::fs` file I/O** — `std::fs::write`/`read`/`read_to_string`, `File::open`/
    `read`/`write`/`seek`/`metadata`, and `fs::metadata`/`exists` work over fsd,
    persisted to ext2. rt shims `__oxbow_fs_open`/`_pread`/`_pwrite`/`_close`
    (positioned, relative to the program's cwd dir cap at slot 1); std backend
    `sys/fs/oxbow.rs`. Verified from the shell: write a file, read it back, stat its
    size, seek + read a slice. Plus `read_dir` + `create_dir` (multi-component paths, .\/.. omitted). Not yet wired: `remove_file`/
    `rename` (stubbed Unsupported), file timestamps/permissions.
  - ☐ The original hardening list:
- **Phase 3 (cont.) — harden.** Native ELF TLS, `Command` stdio piping (spawn-not-fork),
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
