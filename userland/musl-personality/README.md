# oxbow POSIX personality (musl port)

Userland Linux-syscall translation layer that lets **stock musl libc** run on
oxbow, so Linux/BSD applications port with far less effort. The microkernel is
unchanged — this is all userland. Full design + phasing:
[`docs/posix-personality-plan.md`](../../docs/posix-personality-plan.md).

## How it works
musl issues syscalls through `arch/x86_64/syscall_arch.h`. We replace *only* that
file (`syscall_arch.h` here) so every `__syscallN(n, …)` calls
`__oxbow_syscall(n, …)` (`oxbow_syscall.c`) instead of executing a `syscall`
instruction. The dispatcher translates the Linux x86_64 ABI into oxbow capability
syscalls + a few IPC-backed rt shims. Everything else in musl stays stock.

```
app → musl (stock) → __syscallN → __oxbow_syscall → oxbow rt/kernel
```

## Files
- `syscall_arch.h` — drop-in musl override (routes syscalls to the dispatcher).
- `oxbow_syscall.c` — the dispatcher: Linux NR → oxbow.
- `linux_nr.h` — the Linux x86_64 syscall numbers we handle.
- `oxsys.h` — oxbow raw-syscall inline asm + oxbow syscall numbers + rt shim decls.
- `build-musl.sh` — builds vendored musl with the override + compiles the dispatcher.

## Status — Phase 0 complete (scaffolding + kernel primitive)
**Done and building:**
- `SYS_SET_FSBASE` (abi/kernel) — sets the calling thread's FS base at runtime;
  backs musl's `arch_prctl(ARCH_SET_FS)` for TLS. Kernel builds.
- rt[hosted] shims `__oxbow_set_fsbase` + `__oxbow_mmap_anon` (+ existing
  `__oxbow_write`/`_read`/`_getentropy`/`_exit`). rt builds.
- The dispatcher compiles clean for `x86_64-unknown-none` and exports
  `__oxbow_syscall` with the expected refs to the rt shims.

Implemented syscall arms: `write`/`writev`/`read`, `mmap`(anon)/`munmap`/
`mprotect`/`madvise`, `arch_prctl(SET_FS)`, `set_tid_address`/`gettid`/`getpid`,
`sched_yield`, `clock_gettime`, `getrandom`, `futex`, `rt_sigaction`/
`rt_sigprocmask` (no-op), `getuid`-family (root), `exit`/`exit_group`.

**Not yet (next phases — return `-ENOSYS`):** files (`open`/`read`/`stat`/`lseek`
over the namespace + fd table — Phase 2), `fork`/`execve`/`waitpid` (Phase 3),
signal delivery + `termios`/`ioctl` (Phase 4).

## Next step (Phase 1 — first light)
Build vendored musl with `build-musl.sh`, link a tiny `printf("hello")` program
against `libc.a` + `oxbow_syscall.o` + oxbow-rt[hosted] + a crt that bridges
oxbow's entry to musl's `__libc_start_main`, pack into `/bin`, boot, and confirm it
prints and exits. The crt bridge (Linux-style initial stack: argc/argv/envp/auxv
with AT_RANDOM) is the main remaining Phase-1 piece; musl's `configure`/`make` may
also need tweaks to cross-build with clang against bare x86_64.
