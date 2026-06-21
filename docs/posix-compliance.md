# What it would take to make oxbow POSIX-compliant

*Analysis — 2026-06-20. Companion to the Rust-std port and the BSD reference notes
in `docs/bsd-notes/`.*

## TL;DR

oxbow can run a large fraction of POSIX C software **today** (its libc has ~80
functions — full BSD sockets, stdio, `epoll`/`poll`/`select`, `mmap`/`mprotect`,
`getaddrinfo`, `fcntl`, `stat`; `tcc`, `curl`, and a Rust `std` all run on it). But
**true POSIX.1 compliance is fundamentally in tension with oxbow's design**, and the
right answer is almost certainly *not* "make the kernel POSIX." It is:

> **Keep the capability microkernel core. Provide POSIX as a userspace
> compatibility/personality layer** (libc + a small "POSIX personality" server) that
> *translates* POSIX's ambient-authority, fork-based, signal-driven model onto
> oxbow's capabilities.

This is exactly the path taken by every capability/microkernel system that ships a
POSIX story: **seL4** (the CAmkES/sel4-libc + a root task), **Genode** (its `libc`
+ `noux` runtime), **Fuchsia** (POSIX-lite via fdio, deliberately *not* full POSIX),
and even **WSL1** (a Linux personality on the NT kernel). None of them made their
microkernel POSIX; they emulated POSIX above it.

---

## The deep conflict: ambient authority vs capabilities

POSIX is built on three assumptions oxbow rejects by design:

1. **A single global filesystem namespace + ambient authority.** Any process may
   `open("/etc/passwd")` and the kernel decides access by uid/gid/mode bits. oxbow
   has **no global namespace and no ambient authority** — a process can only reach
   what it holds a *capability* for (its cwd dir cap, granted endpoints). There is no
   "open any absolute path" — paths resolve relative to a directory capability.
2. **`fork()`** — duplicate the entire address space + fd table + signal state, then
   `exec()`. oxbow spawns from **raw ELF bytes** (`sys_spawn_bytes`), no
   address-space copy, no inheritance-by-default. This is the single largest gap.
3. **Asynchronous signals** delivered to arbitrary threads, interrupting syscalls
   with `EINTR`. oxbow's IPC is synchronous rendezvous; it has no async signal
   delivery machinery.

Crucially: **#1 is a feature, not a bug.** oxbow's whole value proposition is "no
ambient authority." A literal POSIX implementation would *re-introduce* the global
namespace + uid/gid model that oxbow was built to avoid. So the goal is *source
compatibility for porting software*, not *behavioral identity with a Unix kernel*.

---

## Where oxbow already stands (the good news)

| POSIX area | Status on oxbow |
|---|---|
| stdio (`fopen`/`fread`/`printf`/…) | ✅ works (proven: `tcc`, `curl`) |
| File I/O (`open`/`read`/`write`/`lseek`/`fstat`/`ftruncate`) | ✅ via the fs server (capability-addressed under the hood) |
| BSD sockets (`socket`/`bind`/`connect`/`send`/`recv`/`getaddrinfo`) | ✅ full set, real wire TCP/UDP/DNS |
| `mmap`/`mprotect`/`munmap` (anon) | ✅ present |
| `epoll`/`poll`/`select`/`eventfd`/`timerfd`/`signalfd` | ✅ present (readiness over oxbow's notif/channel prims) |
| Math/string/ctype/locale-ish | ✅ |
| Threads (Rust `std::thread`, futexes) | ✅ real kernel threads + futex sync |
| `getentropy`/`arc4random`-grade RNG | ✅ `SYS_GETENTROPY` |
| Hard links, symlinks, file timestamps, `rename`, `unlink` | ✅ (recent work) |

So the *libc surface* is broad. The gaps are **structural** (the process/signal/perm
model), not "missing functions."

---

## The gaps, hardest first

### 1. `fork()` — the big one (HARD, philosophically loaded)
POSIX programs (shells, build systems, daemons) lean on `fork()`+`exec()`. oxbow has
no fork. Options, in increasing fidelity/cost:
- **(a) `posix_spawn()` only (recommended).** Many modern programs already use
  `posix_spawn`, which maps cleanly onto `sys_spawn_bytes` + fd/cap inheritance
  actions. Patch software to prefer it. Cheap, capability-clean, no COW needed.
- **(b) `vfork()`/`fork()`-without-COW emulation.** Implement `fork` as
  "snapshot a restricted subset of state, run until `exec`." Only correct for the
  fork-then-immediately-exec idiom; breaks fork-as-concurrency. Medium effort.
- **(c) Real COW `fork`.** Duplicate the address space copy-on-write + clone the
  fd/cap table + signal state. This requires a **kernel COW VM** (page-fault-driven
  copy), a per-process fd/handle table the kernel can clone, and is a large, invasive
  change that re-introduces "inheritance by default" — at odds with capabilities.
  Probably never worth it.

**Recommendation: (a) `posix_spawn` + a thin `fork`-for-exec shim (b).** Skip COW
fork. Document that fork-as-concurrency is unsupported; use threads.

### 2. Asynchronous signals (HARD)
`signal`/`sigaction`/`sigprocmask` exist in libc but are almost certainly stubs.
Real POSIX signals need: async delivery to a (possibly blocked) thread, `EINTR` of
in-flight syscalls, a per-process signal mask + pending set, default actions
(SIGKILL/SIGSEGV/SIGCHLD…). In a synchronous-rendezvous microkernel this means:
- a kernel mechanism to *interrupt a blocked thread* and run a userspace handler
  trampoline (oxbow already kills threads via `SHOULD_DIE` checked at preempt — extend
  that to "deliver pending signal" at the same checkpoints);
- mapping faults (SIGSEGV/SIGBUS/SIGFPE) — oxbow's fault handler already kills the
  proc; route it through a registered handler instead;
- `SIGCHLD` on child exit — oxbow already has exit notifications; surface them as a
  signal.
**Effort: medium-high.** A *partial* signal layer (synchronous/fault signals +
SIGCHLD + SIGTERM/SIGINT, no full async-to-arbitrary-thread) covers most real
software and is tractable.

### 3. The permission model: uid/gid/mode vs capabilities (DESIGN DECISION)
POSIX files have `mode` bits + owner uid/gid; `chmod`/`chown`/`umask`/`access`
operate on them. oxbow uses **capabilities** for access control — there are no
ambient permission checks. To satisfy software that *inspects* `st_mode`:
- store mode/uid/gid bits in the ext2 inodes (lwext4 supports them) and report them
  in `stat` — **but do not enforce them** (the capability is the real access control).
  i.e. POSIX perms become *advisory metadata* oxbow honors for compatibility, while
  the capability remains the security boundary. Cheap, honest, keeps the model intact.
- `umask`, `chmod`, `chown` become metadata-only. `access()` answers from the cap +
  the advisory bits.
**Effort: low-medium.** This is the cleanest reconciliation: POSIX perms as
*reported, non-authoritative* metadata.

### 4. The filesystem namespace (DESIGN DECISION)
`open("/abs/path")`, the VFS mount tree, `/dev`, `/proc`, `/tmp`, FIFOs, unix-domain
sockets bound to paths. oxbow resolves paths relative to a **cwd directory
capability** (no global root by name). To run POSIX software unchanged:
- give each process a **root directory capability** that *is* its "/" — the libc
  resolves absolute paths against it. This is `unveil`-like by construction: a process
  literally cannot name anything outside the dir caps it holds. (oxbow already does
  this for cwd; extend to a root cap.)
- `/dev`: synthesize a small device fs (a server) for `/dev/null`, `/dev/zero`,
  `/dev/urandom`, `/dev/tty`. Medium effort.
- FIFOs (`mkfifo`) and unix-domain sockets-on-paths: map onto oxbow pipes/channels
  named in the fs. Medium effort.
**Effort: medium.** The "root cap = /" mapping is the key idea and is capability-clean.

### 5. Process model details (MEDIUM)
Sessions, process groups, controlling terminals, job control (`setsid`, `setpgid`,
`tcsetpgrp`), `waitpid`/`WNOHANG`/`WIFEXITED`, `getppid`. oxbow has process IDs, exit
notifications, and `kill`-via-notif. Building the process-group/session tree + a tty
line discipline (`termios`: raw/cooked mode, `ECHO`, signals-from-Ctrl-C) is needed
for an interactive POSIX shell + job control. **Effort: medium** (mostly a userspace
"process manager" + "tty server" — microkernel-natural).

### 6. `mmap` of files / shared mappings (MEDIUM)
Anonymous `mmap` works; file-backed `MAP_SHARED`/`MAP_PRIVATE` (used by dynamic
linkers, databases, `mmap`-based I/O) needs the VM + fs server to share pages. oxbow
has a shared-frame mechanism already (used for bulk fs/DNS) — generalize it.

### 7. Dynamic linking (`dlopen`/`.so`) (MEDIUM-HARD, optional)
oxbow runs static ELFs. A real POSIX userland (especially desktop software) wants
`ld.so` + shared libraries. Large but well-trodden; arguably skip in favor of static
linking (which OpenBSD and Go also favor for security/simplicity).

---

## A concrete, staged plan (if you pursue this)

**Stage 0 — decide the target.** Not "POSIX.1 certified" (that needs a conformance
suite + XSI + things like message queues, `aio`, that no one ports against). Target
**"POSIX source-compatible enough to build & run the software you care about"** — in
practice, the subset that `musl` + a BSD libc cover, measured by *"does package X
build and run."*

**Stage 1 — `posix_spawn` + advisory perms + root-cap "/".** (low-med) Unlocks most
non-fork software; reconciles perms and namespace capability-cleanly.

**Stage 2 — a "POSIX personality" server.** (med) A userspace process manager owning
the process-group/session tree, `waitpid`, `/dev`, FIFOs, and a tty/termios server
with a line discipline. This is the microkernel-idiomatic home for POSIX semantics —
it keeps the kernel pure and the POSIX cruft in a replaceable server (very Genode/noux).

**Stage 3 — partial signals.** (med-high) Fault signals + `SIGCHLD`/`SIGTERM`/`SIGINT`
delivered at the existing preempt/`SHOULD_DIE` checkpoints. Enough for a shell + most
daemons. Skip full async-to-arbitrary-thread.

**Stage 4 — file-backed `mmap`, then (optional) `dlopen`.** (med / med-hard)

**Stage 5 — a conformance pass.** Run the **Open POSIX Test Suite** and a real
package set (the `pkgsrc`/NetBSD bootstrap is a good portability yardstick — see
`docs/bsd-notes/netbsd-portability.md`) to measure "% builds & runs," not paper
certification.

---

## The honest recommendation

**Do not chase POSIX.1 certification — it would dilute oxbow's defining feature
(zero ambient authority) and cost enormous effort for a checkbox no one rewards.**
Instead build a **POSIX *personality*** as a userspace layer:

- `posix_spawn` not `fork`;
- capabilities as the real access control, POSIX perms as *advisory reported
  metadata*;
- a per-process **root directory capability** that *is* "/" (so `open("/x")` is
  automatically `unveil`-confined — you get OpenBSD's `unveil` security property *for
  free* from the capability model);
- a userspace **POSIX personality server** for the process tree, `/dev`, FIFOs, and
  a tty;
- partial (fault + lifecycle) signals.

That gets you "most POSIX software builds and runs" while *strengthening* rather than
eroding the capability model — porting Unix software ends up **more** confined on
oxbow than on Unix, because every process is born `unveil`'d and `pledge`'d by which
caps it holds. That is the genuinely interesting story here, and it's one only a
capability OS can tell.
