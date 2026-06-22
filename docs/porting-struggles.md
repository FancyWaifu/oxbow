# Porting software to oxbow — what's actually hard

Notes from porting real software (onetrueawk, kilo, dash) to oxbow through the musl
personality. The short version: getting a program to *compile and link* is the easy
20%; the hard 80% is that a from-scratch microkernel doesn't have the dozens of small
behaviors decades of Unix software quietly assume. Each port surfaces a new batch.

This is descriptive, not a complaint — oxbow is a capability microkernel built from
nothing, so every POSIXism is a deliberate decision, not a freebie. The point is to
record *where the friction is* so the next port is faster.

## The layers a port has to pass through

A ported program runs as: **app C → stock musl libc → `syscall_arch.h` override →
`__oxbow_syscall` dispatcher (userland) → oxbow capability syscalls → microkernel.**
Nothing here is Linux. The dispatcher *translates* the Linux syscall ABI musl was
built for into oxbow's capability calls. So "porting" mostly means: discover which
Linux syscall the program needs, then implement a faithful-enough translation.

## 1. The build system fights you before the code does

Real software assumes autotools / a Unix build host:

- **Codegen on the host, compile for the target.** awk needs `maketab` (generates a
  parser table) and a yacc/bison grammar; dash needs four codegen tools
  (`mksyntax`/`mknodes`/`mksignames`/`mkinit`) plus two shell scripts. These must run
  on the *host* to emit `.c`/`.h`, which then cross-compile for oxbow. Every port
  reinvents "run the host build once, then point a `build.rs` at the generated files."
- **`config.h` is for the wrong OS.** Running `./configure` on macOS detects *macOS*,
  not musl. So `config.h` claims `HAVE_SIGSETMASK` (musl lacks it), or sets
  `USE_MEMFD_CREATE` (we don't have memfd). You hand-edit it for musl — and the bugs
  are subtle: dash gates a feature on `#if defined(HAVE_SIGSETMASK)`, so setting it to
  `0` still counts as *defined*. You have to actually `#undef` it.
- **Static fallbacks collide with musl.** Portable programs ship `static inline`
  fallbacks for `memrchr`, `strchrnul`, `mempcpy`, `tee`, `fnmatch`, `memfd_create`…
  guarded by `#ifndef HAVE_X`. musl *has* all of them, so the fallback redeclares a
  libc function → compile error. The fix is to truthfully define every `HAVE_X` for
  musl — but you only find them one compile error at a time.
- **Feature flags that assume a kernel.** dash defaults `JOBS=1` (job control), which
  wants process groups and `tcsetpgrp` — oxbow has neither. You either disable the
  feature or make the program degrade gracefully (dash does: the tty ioctls fail, it
  prints "job control turned off," and runs fine).

## 2. The long tail of missing syscalls

This is the real work, and it never ends — each program needs a slightly different
slice of the ~400 Linux syscalls. You discover them by running the program and
watching it die with `ENOSYS` (we instrument the dispatcher's default case to log the
number). Examples actually hit:

- awk: needed `fstat`/`stat` via the **x86-64 kstat** path (not `statx` — musl picks a
  different syscall per arch), plus `getdents` later.
- kilo: needed `ftruncate` (editors save via `ftruncate` + `write`).
- dash: needed `getppid`, `fcntl` (the `F_DUPFD` fd-juggling shells do around
  redirections), `getdents64`, `poll`.

Each one is small, but the *faithfulness* matters: a stub that returns 0 instead of
the right value sends the program down a wrong path that's miserable to debug.

## 3. The bugs that aren't "missing," they're "subtly wrong"

These cost the most time, because the program runs and just misbehaves:

- **Two return values.** oxbow syscalls return *two* registers (rax + rdx); Linux
  returns one. A musl helper that didn't mark rdx as clobbered let the compiler reuse
  a now-garbage rdx across a call — a heisenbug that debug prints *hid* (they spilled
  the register).
- **TLS installed by raw assembly.** musl installs its thread pointer with a
  hand-written `arch_prctl` syscall in `.s`, which *bypassed* the C syscall override
  entirely. So `fs` stayed on the kernel's bare TLS block, `self->locale` was NULL,
  and the first `setlocale` segfaulted. One program (awk) needed it; another
  (muslhello) had survived for five phases purely because it never touched
  `self->locale`. Required overriding the one assembly file.
- **Shared vs. owned handles.** Shells `dup` a fd then `close` the original. Our
  `dup2` shared the underlying fs handle, so closing the original killed the dup — and
  `sh script` silently read nothing. Fixed by refcounting the shared handle.
- **Wrong ioctl argument.** A `TCSETS` handler read the termios struct from `a2`, but
  in `ioctl(fd, req, arg)` the pointer is `a3` (`a2` is the request number) — so it
  dereferenced `0x5404` and page-faulted. Found only from the fault address.

## 4. The capability model doesn't have the concepts POSIX assumes

The deepest friction: some POSIX behaviors don't *map* onto a pure-capability kernel,
so you have to build the concept:

- **Pipe EOF.** A POSIX pipe signals EOF when the last writer closes — which requires
  the kernel to refcount write ends across `fork`/`dup`/`exec`. oxbow pipes originally
  had no writer refcount (EOF was an explicit call), so anything fork-based —
  pipelines, `$(command substitution)` — read empty or hung. The fix is a real kernel
  feature.
- **Signals.** There's no ambient "send SIGINT to the foreground process group." We
  had to build: a kernel notion of the controlling-tty foreground process, a way to
  raise a signal cross-process, signal-frame *injection* on the return-to-user path,
  and a `sigreturn` — to make Ctrl-C interrupt a running program and run its handler.
- **Process groups / job control, controlling terminals, `/bin/sh` for `popen`.**
  Either built, stubbed to degrade, or still pending.
- **No arg boundaries.** oxbow passes argv as one space-joined string, so an argument
  containing a space can't survive — `awk '{print $1}'` has to become `awk -f prog`.

## 5. Testing is its own tax

There's no `ssh` in; tests drive QEMU via QMP keystrokes and scrape the serial log.
The keyboard can't even *type* `;`, `$`, `(`, `)`, `*` without shift-handling, so shell
scripts must be tested from files, not typed. Each iteration is a full build + ISO +
(sometimes) a multi-second disk reseed + boot — minutes per attempt, and the
emulator's flakes (a killed boot corrupts the disk seed) are real.

## So why does it take so much?

Because every one of these is a thing Linux gives software for free and a from-scratch
microkernel does not. The *good* news is that the work compounds: TLS, the kstat path,
fork, pipes-with-EOF, signals, getdents, fcntl, the build harness — each was paid for
once and now every future port starts further along. awk took a lot of new ground;
kilo reused most of it and only added raw-mode tty + `ftruncate`; dash reused almost
everything and mostly needed `fcntl` + a handle-refcount fix. The curve is bending the
right way — the personality is slowly accumulating the "boring" 80% of Unix that real
software depends on.
