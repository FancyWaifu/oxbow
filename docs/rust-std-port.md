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
    `SYS_THREAD_ID`(57), `SYS_YIELD`(58). Limitations: TLS destructors don't run (guard is
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
    size, seek + read a slice. Plus `read_dir` + `create_dir` (multi-component paths, .\/.. omitted). Plus `remove_file`/`rename`/`remove_dir`/`remove_dir_all`. Not yet wired: symlinks, timestamps/permissions.
  - ☐ The original hardening list:
  - ✅ **`std::process::Command`** — `Command::new(prog).status()`/`.spawn()`+`wait()`
    spawns a child (std reads its ELF via std::fs), inheriting the parent's stdio/cwd/
    net caps, and returns its exit code. Verified: a std parent spawns a std child,
    the child prints (inherited stdio) and exits 42, the parent reads code Some(42).
    rt shims `__oxbow_spawn`/`__oxbow_wait` (sys_spawn_bytes + an exit notif).
    The program is resolved relative to the cwd cap (the user namespace) — /bin (the
    shell's tools) is NOT reachable from a user process, by the capability model.
  - ✅ **Piped `Command::output()`** — captures a child's stdout + exit code over a
    kernel pipe. `Command::spawn` wires `Stdio::MakePipe` (grant the pipe write-end to
    the child as its stdout, keep the read-end), `output()` waits → `sys_pipe_eof` →
    drains (the kernel pipe has no writer-refcount, so EOF is an explicit call). rt
    shims `__oxbow_pipe`/`_read`/`_write`/`_close`/`_eof`; std `sys/pipe/oxbow.rs`.
    Verified: parent gets `56 bytes, exit Some(7)` from a child that prints 2 lines.
  - ✅ **`Command::try_wait`** — non-blocking child-exit check via `SYS_NOTIF_POLL`
    (rt `__oxbow_try_wait`). It drains the exit signal, so `Process` caches the status
    (`exited`) and a later blocking `wait()` returns the cache instead of deadlocking
    on the drained notification. Verified: `running` → `wait=Some(5)` → cached
    `try_wait=Some(5)`.
  - ✅ **`Command::kill`** — SMP-safe cross-process kill, capability-clean: authority
    is holding the child's **exit-notif** handle (the spawn-time lifecycle handle —
    no ambient pid). `SYS_PROC_KILL(notif, code)` finds the child by that notif, reaps
    it (`proc::kill`: close handles, free budget, signal the notif so `wait()` gets the
    code), and flags its threads (`SHOULD_DIE`) to **self-terminate** at safe points
    (`preempt` for Running/Ready, after `block_current` for Blocked — blocked threads
    are woken to reach it). Self-exit (never an external force-Exited) is the only
    SMP-safe way — it avoids the kernel-stack-reuse race. `proc::create` won't reuse a
    Dead slot with live threads (prevents the address-space UAF). Verified: a spinning
    child AND a parked child both → `wait()=Some(137)`, parent + system survive.
  - ✅ **`panic=unwind`** — real DWARF stack unwinding. The pure-Rust `unwinding`
    crate (fde-static) supplies the Itanium `_Unwind_*` ABI; `library/unwind` routes
    oxbow through the xous-style binding (its own types — ABI-safe), and oxbow was
    added to the `panic_unwind` + `sys/personality` gcc arms (so `rust_eh_personality`
    exists). The target dropped `panic-strategy: abort`; the linker script keeps
    `.eh_frame`/`.gcc_except_table` + defines `__executable_start`/`__etext`/`__eh_frame`
    for the static FDE finder (no runtime registration → fits `#![no_main]`). Build a
    program with `-Z build-std=std,panic_unwind` + profile `panic = "unwind"`. Verified:
    `catch_unwind` catches a panic and the program continues; a panicking thread's
    `join()` returns `Err` and the **process survives** (panic isolation).
  - 🐛 **Fixed a kernel-panicking std bug:** `std::process::exit` had no oxbow arm in
    `sys/exit.rs` → fell into `_ => intrinsics::abort()` → `ud2` → a userland #UD on
    *every* exit, which the kernel's `invalid_opcode` handler escalated to a full
    kernel panic (silent freeze). Fixes: (1) `sys/exit.rs` oxbow arm → `__oxbow_exit`;
    (2) kernel `idt.rs` — a ring-3 #UD now `kill_current_user()`s (like #PF) instead
    of panicking, so a bad user instruction can't take down the machine.
  - ✅ **Native ELF TLS** (replaces the keyed `(tid,key)`-table hack). Kernel: the
    ELF loader captures `PT_TLS` (`elf.rs`); each thread gets its own TLS block built
    from the template (x86-64 variant II — `.tdata`/`.tbss` below the thread pointer,
    TCB self-pointer at it) by `proc::build_tls_block` (main thread in `load_into`,
    spawned threads in `build_thread_tls`); the block's thread pointer is stored in
    `Tcb.fs_base` and loaded into `IA32_FS_BASE` on every `switch_to` (`arch::set_fs_base`).
    Userland: `has-thread-local: true` in the target spec flips `cfg(target_thread_local)`
    so std's `thread_local!` uses the **native** backend (no std-source change). Std
    programs need the TLS-aware linker script (`std-port/user-tls.ld` — adds a
    `tls PT_TLS` PHDR + `.tdata`/`.tbss`). Verified: raw `#[thread_local]` AND
    `thread_local!` give per-thread isolation across main + spawned threads
    (`main=1`, each spawned thread sees the fresh template then its own write).
    The keyed `sys/thread_local/key/oxbow.rs` is now dead (left in place).
  - ✅ **TLS destructors** — a `thread_local!` holding a `Drop` type now runs its
    destructor when a spawned thread exits. oxbow has no automatic thread-exit
    callback (its `guard::enable` is a no-op), so `sys/thread/oxbow.rs::thread_start`
    calls `destructors::run()` + `rt::thread_cleanup()` after the closure, before
    `__oxbow_thread_exit`. Verified: `Dropper(7) dropped at thread exit` prints
    between the thread's work and the join. (Main-thread TLS dtors at process exit
    still leak — the whole AS is torn down, so it's moot.)
- **Phase 3 — DONE.** native TLS, TLS destructors, Command try_wait/kill, panic=unwind
  all landed and verified.
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
