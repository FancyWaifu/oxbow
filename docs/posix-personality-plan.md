# oxbow POSIX personality — porting musl libc

**Goal:** make porting Linux/BSD applications to oxbow dramatically easier by
giving them the libc + syscall surface they expect, *without* turning the
microkernel into a POSIX kernel. We do this by porting **musl libc** and riding
it on a **userland Linux-syscall translation layer** that maps the Linux ABI onto
oxbow's capability rt.

This keeps oxbow's thesis intact: the kernel stays a tiny capability microkernel;
the POSIX/Linux personality is *userland code* that ported programs link against.
Native oxbow software keeps using the clean capability API; ported software sees
a Linux-shaped libc. Two tiers, one kernel.

Precedent: this is exactly how Fuchsia (POSIX-lite over a capability kernel),
WSL1 (Linux syscalls over the NT kernel), and gVisor (Linux ABI in userland) work.

---

## 1. Why musl, and why a syscall-translation port

We have been growing oxbow-libc by hand (strndup, memmem, isblank, bsearch, …).
That scales for coreutils but not for real applications. musl gives us a large,
correct, spec-tested POSIX surface *at once*. The cost is that musl assumes a
roughly-Linux syscall ABI — which is a *feature*: adopting musl forces us to build
the personality (fds, fork/exec, signals, namespaces) underneath, which is the
exact work that unblocks everything else.

**Port strategy = Linux-syscall emulation (not a source fork).** musl issues
syscalls through `arch/x86_64/syscall_arch.h`, which today does `__asm__("syscall")`.
We replace *only that file* so each `__syscallN(n, …)` calls a plain C function:

```c
long __oxbow_syscall(long n, long a1, long a2, long a3, long a4, long a5, long a6);
```

Everything above the syscall line in musl — `errno`, stdio, malloc (mallocng),
pthreads, string/math, the dynamic-ish startup — stays **stock**. All the porting
work lives in one userland dispatcher that translates Linux syscall numbers into
oxbow rt calls. No real `syscall` instruction is ever issued by ported code.

```
   ported app  (Linux/BSD C source, unmodified)
        |
   musl libc   (stock 1.2.5, except syscall_arch.h)
        |  __syscallN(n, a1..a6)
        v
   __oxbow_syscall(n, ...)        <-- THE personality (userland C)
        |   switch (n) { translate Linux ABI -> oxbow }
        v
   oxbow rt  (capability syscalls: SYS_MAP, SYS_FUTEX_*, fs IPC to fsd, ...)
        |
   oxbow microkernel  (unchanged: pure capabilities)
```

musl libc and oxbow-libc are **mutually exclusive** in a given binary (both define
malloc/printf/…), exactly like the Rust `std` port vs oxbow-libc. A program is
either "native oxbow-libc" or "musl/POSIX". New ports choose musl.

---

## 2. What oxbow already gives us (the easy half)

The rt/kernel already expose primitives that map cleanly onto Linux syscalls:

| Linux syscall            | oxbow primitive                                   |
|--------------------------|---------------------------------------------------|
| `write`/`read`/`writev`  | fs IPC to fsd (`rt::fs`), tty write, `SYS_PIPE_*` |
| `mmap`/`munmap`          | `SYS_MAP` / `SYS_PROTECT` / frame alloc           |
| `mprotect`               | `SYS_PROTECT`                                      |
| `exit_group`/`exit`      | `SYS_EXIT`                                         |
| `clone`(thread)/`futex`  | `SYS_THREAD_SPAWN` + `SYS_FUTEX_WAIT/WAKE`        |
| `gettid`/`set_tid_addr`  | `SYS_THREAD_ID`                                    |
| `sched_yield`            | `SYS_YIELD`                                        |
| `getrandom`              | `SYS_GETENTROPY`                                   |
| `clock_gettime`          | `SYS_WALLTIME` (RTC) + `SYS_UPTIME_MS` (monotonic)|
| `dup`                    | `SYS_CAP_DUP`                                      |
| `pipe`/`pipe2`           | `SYS_PIPE`                                         |
| fd-passing               | `SYS_CHANNEL_SEND/RECV` (caps), `SYS_CAP_TYPE`    |

So threads, locks (futexes), memory, time, and entropy are *already there*. That
covers a surprising fraction of a real program's syscalls.

## 3. What we must build (the hard half)

### 3.1 Thread pointer / TLS (the first bring-up blocker)
musl startup (`__init_tls` → `__set_thread_area`) sets the x86_64 FS base for TLS;
`errno`, stdio locks, and pthread state are all TLS. The kernel already builds a
TLS block and sets `fs_base` from `PT_TLS` at ELF load (`proc::build_tls_block`),
but musl wants to set the TP itself at runtime.

**Action:** add a tiny kernel syscall `SYS_SET_FSBASE(addr)` that sets the calling
thread's FS base (writes `MSR_FS_BASE` / uses `wrfsbase`). Translate
`arch_prctl(ARCH_SET_FS, addr)` → `SYS_SET_FSBASE`. This is the only strictly
required *kernel* change for first light. (Alternatively enable FSGSBASE and let
musl `wrfsbase` directly, but a syscall is simpler and keeps the kernel in
control.)

### 3.2 Per-process file-descriptor table
POSIX programs wire stdin/stdout/stderr and pipes via integer fds and expect them
inherited across exec. oxbow passes capabilities positionally at spawn (cap0 @
slot 1, stdout @ slot 2) and has an `FdSlot` in oxbow-libc already.

**Action:** the personality owns an fd table mapping `int fd -> { kind, cap
handle, offset }`. fd 0/1/2 are bound from the caps the spawner grants. `open`
allocates an fd backed by an fsd file handle; `close`/`dup2`/`fcntl(F_DUPFD)`
manipulate the table; `read`/`write`/`lseek` dispatch on fd kind (file vs pipe vs
tty). fd inheritance across spawn = serialize the table's caps into the spawn
message (we already pass caps at spawn).

### 3.3 Namespace / path resolution (reconciles "no ambient authority")
POSIX code calls `open("/usr/lib/x")`. oxbow has no global root. Resolve absolute
paths against a **per-process namespace** — a set of (prefix → dir-cap) mounts the
spawner grants (e.g. `/` → BOOT_FS_ROOT, `/bin` → the shared bin cap). The
namespace *is* a held capability, so there is still zero ambient authority: a
process can only reach what its namespace names. `openat(dirfd, rel)` resolves
against the dirfd's cap. This is the Fuchsia model.

**Action:** a `namespace_resolve(path) -> (dir_cap, residual)` in the personality,
seeded from spawn. `open`/`stat`/`access`/`*at` go through it.

### 3.4 fork/exec
The dominant real use is `fork()`-then-`exec*()`. We do **not** need COW fork for
that.

**Action:**
- `posix_spawn` path: implement `fork`+`execve` as a deferred spawn — `fork`
  returns a "pending child" token, recording the file-actions the child wants
  (dup2/close on fds); `execve` materializes it via `SYS_SPAWN_BYTES` with the
  fd-table caps + namespace in the spawn message. Covers shells, configure, make.
- `vfork` → same path.
- True COW `fork` (child keeps running without exec): deferred. Needs AS
  snapshot; rare enough to punt.
- `waitpid` → block on the child's exit-notif (`SYS_NOTIF_*`), already how the
  shell waits.

### 3.5 Signals (subset)
**Action (phase 2):** deliver `SIGINT`/`SIGTERM`/`SIGCHLD`/`SIGPIPE`/`SIGSEGV`.
`sigaction`/`sigprocmask` manage a per-process handler/mask table in the
personality; the kernel gains a way to deliver an async upcall (or we ride the
existing notif mechanism + a userland signal pump on a dedicated thread). Until
then `sigaction`/`sigprocmask` are accepted as no-ops so programs that merely
*install* handlers still run.

### 3.6 The long tail (accept-or-stub first, implement on demand)
`ioctl` (TCGETS/TIOCGWINSZ for isatty/term size), `fcntl` flags, `termios`,
`uname`, `getpwnam`/`getuid` (synthetic single-user), `stat` mode-bit synthesis
(report perms from "do I hold a cap"), env vars, symlinks/FIFOs. Each is a switch
arm added when a target app demands it.

---

## 4. Build integration

- musl source lives **out of repo** at `~/musl-oxbow/musl-1.2.5` (like the Rust
  std fork at `~/rust-oxbow`), vendored, not committed.
- The oxbow side (committed) is `userland/musl-personality/`:
  - `syscall_arch.h` — the drop-in that routes `__syscallN` → `__oxbow_syscall`.
  - `oxbow_syscall.c` — the dispatcher.
  - `linux_nr.h` — the Linux x86_64 syscall numbers we handle.
  - `crt_glue.c` / startup shims as needed.
- Build musl with our custom arch header on the include path ahead of musl's, or
  by overwriting `arch/x86_64/syscall_arch.h` in the vendored tree (documented in
  a `build-musl.sh`). Configure: `--target=x86_64 --disable-shared` (static only),
  no dynamic linker.
- A musl-linked test program (`muslhello`) is built with musl's `musl-gcc`-style
  wrapper pointing at oxbow's clang + the oxbow `__oxbow_syscall` object, then
  packed into `/bin` like any other tool. It is a *static* ELF; oxbow's loader
  already runs static ELFs with `PT_TLS`.

---

## 5. Phasing (each phase is independently verifiable on QEMU)

**Phase 0 — scaffolding (this doc + skeleton).** Vendor musl; write
`syscall_arch.h` override + `__oxbow_syscall` dispatcher skeleton + `linux_nr.h`;
add the `SYS_SET_FSBASE` kernel syscall. *Done when the dispatcher + override
compile.*

**Phase 1 — first light: a stock-musl "hello world" prints and exits.** Minimal
syscalls: `arch_prctl(SET_FS)`, `set_tid_address`, `brk`+`mmap` (mallocng),
`writev`/`write`, `ioctl(TIOCGWINSZ/TCGETS)` (stub for isatty), `rt_sigprocmask`
(no-op), `exit_group`. *Done when `muslhello` prints from `printf` and exits 0.*

**Phase 2 — files + fd table.** `openat`/`open`/`read`/`write`/`lseek`/`close`/
`fstat`/`stat`/`dup`/`dup2` over the namespace + fd table. *Done when a musl
program reads /hello.c and `fstat`s it correctly.*

**Phase 3 — process: fork/exec/wait.** posix_spawn-style fork+execve+waitpid.
*Done when a musl program spawns another and collects its exit code.*

**Phase 4 — signals + termios subset.** Ctrl-C, SIGCHLD reaping, isatty/winsize,
canonical/raw mode. *Done when a musl line-editing REPL works.*

**Phase 5 — port a real app.** Target: a small editor (kilo) or `ed`, then
busybox-style tools, then a `./configure`-using package. *Done when an unmodified
upstream tarball builds-or-runs.*

---

## 6. First kernel change (required for Phase 1)

`SYS_SET_FSBASE` (one new syscall): set the calling thread's FS base to `arg0`.
The kernel already stores a per-thread `fs_base`; this just lets userland update
it at runtime so musl's `__set_thread_area` works. Everything else in Phase 1 is
userland.

---

## 7. Risks / open questions

- **mallocng vs brk:** musl 1.2.5's allocator prefers `mmap`; ensure `SYS_MAP`
  semantics (anonymous, fixed/hint, prot) cover what mallocng asks for.
- **Static TLS sizing:** kernel `build_tls_block` must match musl's TLS layout, or
  we let musl own TLS entirely and the kernel only provides a scratch initial TP.
- **errno:** musl maps kernel negative returns to errno; `__oxbow_syscall` must
  return Linux-style `-errno` on failure, so we need an oxbow-error → Linux-errno
  table.
- **`syscall` cancellation points:** musl's pthread cancellation wraps some
  syscalls; fine as long as `__oxbow_syscall` is a normal call.
- **Two libcs:** musl programs cannot link oxbow-libc; the build must keep them
  separate (distinct sysroot/include path), as with the std port.
