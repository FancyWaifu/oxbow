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

## Status — Phase 3b reached: REAL fork() + exec + wait ✅
`fork()` works via a kernel address-space clone. `muslhello` forks a child that
`_exit(42)` and the parent reads exactly 42, then forks+execs `/bin/seq`:
```
  --- fork + _exit(42) ---
  fork#1: pid=11 waited=11 exit=42 (expect 42)
  --- fork + exec `seq 1 5` ---
  1 2 3 4 5
```
How: `SYS_FORK` clones the parent's lower-half address space into a new process
(`mm::vm::clone_user_as` — eager copy at the same virtual addresses, shared frames
re-mapped) + copies the handle table (fd/cap inheritance) with its own Memory
budget; the child's main thread starts at a trampoline that `longjmp`s to the fork
point in its OWN copied AS (setjmp/longjmp does the register save/restore — same VA
in the copy, so it Just Works). `waitpid` blocks on the child's exit-notif.

Two bring-up bugs found + fixed: (1) the child's exit-notif handle reached `SYS_FORK`
as 0 because oxbow returns handles in RDX (rax=status), but `ox_syscall0` read rax;
(2) — the heisenbug — oxbow syscalls return TWO values (rax+RDX) unlike Linux, so
RDX is always clobbered, but the Linux-derived `ox_syscallN` asm didn't mark it, and
the compiler reused a now-zeroed RDX (`a3`=the notif handle) across `SYS_FORK`. Now
all helpers treat RDX as an output (`+d` for the a3 ones). The earlier vfork attempt
(shared stack) is superseded — a separate AS is what makes fork correct.

## Status — Phase 3a: EXEC + SPAWN + WAIT ✅
`muslhello` execs `/bin/seq`, whose stdout it inherits, and exits with its status:
```
  --- execve("/bin/seq", {seq,1,5}) ---
1
2
3
4
5
```
Working: `execve` (reads the target ELF via fsd, spawns it with `__oxbow_spawn`
inheriting cwd+stdout, runs to completion, exits with its status — the launcher /
"exec as the last thing" case), and `waitpid` (`__oxbow_wait` on the child's
exit-notif). Reuses the existing `std::process::Command` shims; no kernel/rt change.

**True `fork()` — Phase 3b ATTEMPTED, needs a separate-AS COW fork (deferred).**
`fork`/`clone` return `-ENOSYS` for now so fork-using code takes its failure path.

A full vfork-style kernel primitive was built and tested (`SYS_VFORK_SPAWN`/
`SYS_VFORK_RESUME`): userland `setjmp`/`longjmp` captures the context (so the kernel
needs no register capture), the kernel spawns a child thread sharing the parent's AS
and suspends the parent, the child `longjmp`s onto the parent's idle stack, runs up
to `execve`, spawns the program, and resumes the parent with the child pid. It got
*almost* all the way — verified on QEMU: child spawns, `seq` runs, parent is woken
to Ready — but it **fundamentally can't work with a shared stack**. musl's `_Fork`
does substantial work (atfork handlers, lock resets) and `main`+`execve` run a deep
call chain ON THE SHARED STACK, clobbering the parent's *suspended call chain*
(`NR_fork→__oxbow_syscall→__syscall→_Fork→fork→main`). A scratch-stack switch
protects the deepest frame but not those. The classic vfork contract ("child does
*only* `exec`") is violated by musl itself.

**The real fix is a separate address space.** `fork` must copy the parent's AS (eager
copy or COW) at the *same* virtual addresses, then run a child thread in that copy
resuming at the parent's RIP with `rax=0`. The `setjmp`/`longjmp` trick then works
unchanged (the copied stack lives at the same VA), and the child never touches the
parent's memory. That's a major kernel feature (per-fork AS clone + COW page faults)
— a focused future effort. The bring-up learnings (stack alignment for raw thread
entries, the lost-wakeup-safe block/wake, the scratch-stack switch) carry over.

## Status — Phase 2: HEAP + STDIO + FILE I/O ✅
`muslhello` now exercises the full picture on the hardware-path QEMU:
```
Hello from musl libc, running on oxbow!
  sum(1..10) = 55 via stock musl printf
  malloc + snprintf at 0x40002040
  stat(/hello.c): 510 bytes
  first line: /* A C program to compile + run ON oxbow ...
  readback: written by musl libc on oxbow
```
Working: `malloc`/`free` (mallocng over `mmap`), buffered stdio
(`fopen`/`fgets`/`fread`/`fprintf`), and file I/O over fsd — `open`/`openat`/
`read`/`write`/`lseek`/`close` plus `stat`/`fstat`/`lstat`/`fstatat` (filling the
x86_64 `struct kstat`; musl skips `statx` on x86_64). Paths resolve against the
process's cwd dir cap via `__oxbow_fs_*`, so there is still zero ambient authority.

Two bring-up bugs found + fixed: (1) musl's stat uses the **kstat** path on x86_64,
not statx; (2) mallocng's meta-area setup mmaps `PROT_NONE` then `mprotect`s to RW
and bails if mprotect fails — our anon mappings are already RW, so `mprotect`→0 and
`brk`→a fixed (never-growing) address let mallocng fall back to mmap cleanly.

## Status — Phase 1: STOCK MUSL RUNS ON OXBOW ✅
`servers/muslhello` is a `printf` program compiled against musl headers, linked
with the freshly-built musl `libc.a` + the crt bridge + oxbow-rt[hosted]. On the
hardware-path QEMU it prints and exits cleanly:
```
root@oxbow:/$ muslhello
Hello from musl libc, running on oxbow!
  sum(1..10) = 55 via stock musl printf
```
Full chain verified: oxbow `_start` → `oxbow_main` (crt_glue: synthesizes the
Linux initial stack + auxv incl. AT_RANDOM) → musl `__libc_start_main` → musl TLS
init (`arch_prctl(ARCH_SET_FS)` → `SYS_SET_FSBASE`) → musl stdio `printf` →
`writev` → `__oxbow_syscall` → oxbow tty. musl `libc.a` has 254 objects routed
through `__oxbow_syscall`.

Build: `userland/musl-personality/build-musl.sh` builds musl with the override;
`cargo build -p muslhello` links a program. muslhello is in the `_iso` /bin loop
(kept OUT of the default `build-server`, since it needs the out-of-repo musl).

## Status — Phase 0 (scaffolding + kernel primitive)
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
