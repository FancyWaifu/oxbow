# oxbow userland command port status

A complete-as-practical list of the standard Unix/BSD userland, tracked against
oxbow. oxbow's model differs from POSIX in two load-bearing ways, so some classic
commands are *intentionally* absent rather than merely unported:

- **No ambient authority.** A process can only touch what it holds a capability
  for. `cwd` is slot 1; `/bin` is a shared dir cap handed out at login. There is
  no global namespace a process can walk uninvited.
- **No permission bits.** Access *is* possession of a capability. There is no
  `mode` to change, so `chmod`/`chown`/`chgrp`/`umask` have no referent.

Tiers: **HAVE** (works today) · **NEXT** (portable with no kernel change) ·
**LIBC** (needs a libc/syscall addition first) · **N/A** (doesn't fit the model).

---

## HAVE — shipping today

### Shell builtins
`echo` · `cd` · `pwd` · `run` · `exec` · `exit` · `help` · `sync` ·
`whoami` · `id` · `groups` · `su` · `passwd` · `dns` · `http` · `curl` ·
`tcc` · `cc` · `lua` · `micropython` · `qjs` (language runtimes)

### /bin programs
- **files:** `ls` `cat` `cp` `mv` `rm` `mkdir` `touch`
- **text:** `wc` `head` `tail` `grep` `cut` `rev` `fold` `comm` `uniq` `tr`
  `paste` `tee` `split` `od` `strings` `printf` `cmp`
- **find/glob:** `find`
- **shell-trivial:** `true` `false` `yes` `seq` `basename` `dirname` `sleep`
- **process:** `ps` `kill`  (via the `PLEDGE_PROC` syscall API: `SYS_PROC_LIST`,
  `SYS_KILL`)

Ported from **sbase** (suckless) verbatim where one existed — we vendor the real
sbase sources + its `libutf` (UTF-8) + `libutil`, compiled with clang against
oxbow-libc. Only the support shims (`oxcompat.c`) and a lean `util.h` are ours.

---

## NEXT — portable now, no kernel change needed

These need only more libc surface we can add in userland, or are pure-compute:

- **sort** — needs `getlines` (have) + qsort (add to libc; trivial).
- **nl** `expand` `unexpand` `tac` — pure text, sbase/coreutils.
- **cksum** `md5sum` `sha1sum` `sha256sum` `sha512sum` — sbase ships the crypto
  (`md5.c`/`sha*.c` in libutil); just vendor those .c files.
- **expr** `test`/`[` — pure compute (string/int eval).
- **date** `uname` `hostname` `arch` `uptime` — read kernel/boot info; `uname`
  can be a static string today, `date`/`uptime` use `ox_uptime_ms` (have).
- **env** `printenv` — oxbow has no env vars yet; trivial once a (per-process,
  capability-scoped) environment is wired. Shell already does `$VAR`.
- **which** — resolve a name against `/bin` (have the dir cap).
- **xargs** — spawn + arg batching; uses the spawn path we already have.
- **cal** `factor` `primes` `tsort` `shuf` — pure compute.
- **du** (single-dir), `wc -L`, `cat -v` — variants of tools we have.

## NEXT (large, but no kernel change) — the heavy text tools
- **sed** · **awk** (onetrueawk/goawk-C) · **ed** · **diff** · **patch** ·
  **m4** — all pure userland C. Bigger ports; sed/awk are the highest-value.
- **tar** · **cpio** · **gzip**/**gunzip** · **compress** — archive/stream; tar
  needs directory recursion (see LIBC/openat below) but the format code is pure.

---

## LIBC — needs a libc/syscall addition first

- **Directory recursion** (`ls -R`, `cp -r`, `rm -r`, `find` deep, `du`, `tar`):
  oxbow has `opendir`/`readdir` now, but recursive descent wants `openat`-style
  "open child *relative to a dir cap*" so each subdir is reachable without a
  global path. This is the one real gap; it's a capability-clean syscall to add
  (`open_at(dir_cap, name) -> cap`). Once present, the recursive variants light up.
- **ln** `link` `unlink` `rmdir` `readlink` `mkfifo` `mknod` `mktemp` `truncate`
  — each maps to an fs-server op we have to expose (most are small).
- **stat**/`statvfs` CLI — the `stat()` libc call exists (used by `tail`/`find`);
  the user-facing `stat`/`df` commands just need formatting.
- **time** `nice` `nohup` `timeout` — need the process API to grow (priority,
  detach, wall-clock kill); `kill`/`ps` are the first slice of this.

---

## N/A — doesn't fit oxbow's capability model

- **chmod** `chown` `chgrp` `umask` — no permission bits; access = holding a cap.
- **chroot** — every process is already namespace-confined to its caps; there is
  no ambient root to escape from, so chroot is a no-op concept.
- **mount**/`umount` (as ambient ops) — mounting is granting a cap to a server,
  done by the spawner, not a global syscall.
- **setuid/sudo-as-setuid** — `su` exists but as a credential-authority handoff
  (the shell mints caps), not a setuid bit. No setuid binaries by design.

---

### How a new tool gets ported (the pipeline)
1. `cp` the verbatim sbase/coreutils `.c` into `userland/sbase/`.
2. Add a thin crate `servers/<tool>/` (`Cargo.toml` + `main.rs` =
   `#![no_main] extern crate oxbow_libc as _;` + the shared `build.rs`).
3. The shared `build.rs` compiles the tool + globbed `libutf/`+`libutil/` against
   oxbow-libc and links `user.ld`. Fill any missing libc symbol in `oxcompat.c`.
4. Add the crate to `Cargo.toml` workspace + the `just build-server` loop + the
   `_iso` `/bin` loop. It's now a file in `/bin`, reachable by every user.
