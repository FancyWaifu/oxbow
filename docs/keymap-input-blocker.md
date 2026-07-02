# Keyboard input in the Weston desktop — the keymap fd cascade (SOLVED)

Status: **fixed.** havoc (and any Wayland client) now receives its xkb keymap, so keystrokes
become characters. Getting here meant peeling a three-bug cascade; recorded so the shape is
remembered.

## Symptom
On boot: `creating a keymap file failed: Out of memory` (weston `libweston/input.c:2102`), then a
dead terminal you can't type in (`sh: can't access tty; job control turned off`, no echo). The
window rendered but the keyboard did nothing.

## The cascade (each bug hid the next; all found via instrumented headless boots)

1. **memfd-munmap freed the region.** weston's `os_ro_anonymous_file_create()` does
   `memfd → fallocate → mmap → memcpy(keymap) → munmap`, then keeps `file->fd`. oxbow's `NR_munmap`
   called `SYS_CLOSE` on the region cap → **freed the memfd**, so `file->fd` was dead afterward and
   the keymap couldn't be re-read → ENOMEM. POSIX: munmap unmaps, only `close(fd)` frees a memfd.
   **Fix:** `NR_munmap` frees a K_SHM region only if no open fd still references it (symmetric with
   `close(K_SHM)`, which frees only if no mapping is live). Region lives until BOTH are gone.

2. **The keymap fd was closed before libwayland flushed it.** With #1 fixed the keymap succeeded,
   but havoc still stalled at `toplevel added` and received NO further events. The channel trace
   showed the keymap event's wire bytes reaching havoc with **`caps=0`** — the fd was missing, so
   libwayland errored the connection and dropped every later event (including the `xdg_surface`
   `configure` havoc needs to draw). Root: weston seals the keymap memfd read-only so
   `os_ro_anonymous_file_put_fd` will NOT close it before the flush — but oxbow didn't track seals,
   so `F_GET_SEALS` returned 0, `put_fd` closed the fd early, and `sendmsg` found nothing to pass.
   **Fix:** track memfd seals (`F_ADD_SEALS`/`F_GET_SEALS` on K_SHM fds).

3. **havoc used the copy path that seals can't save.** Seal tracking only helps the *cheap* path
   (`os_ro_anonymous_file_get_fd` returns the sealed fd directly), which weston takes only for
   `wl_keyboard` **version ≥ 7** (`MAPMODE_PRIVATE`). havoc bound `wl_seat` at **v5** → weston used
   `MAPMODE_SHARED` → the expensive path made a fresh *unsealed* copy fd that `put_fd` closed early
   → same `caps=0` desync. weston advertises up to v7 (`input.c:3429 MIN(version,7)`).
   **Fix:** havoc binds `wl_seat` at version 7 (`~/musl-oxbow/havoc/main.c`).

4. **THE ROOT CAUSE — capability-transfer refcount bug.** Even with #1-3, havoc's `mmap` of the
   received keymap fd FAILED with the region showing **`size=0`** (freed). `kernel/src/channel.rs`
   `sys_channel_send` **copies a cap's HandleEntry into the channel WITHOUT increfing** the shm
   region (line ~2180, `p.get`). So the in-flight cap is an *uncounted* reference: when weston's
   `put_fd` closes its keymap fd (right after the send, before havoc receives), the region's rc
   hits 0 and it's **freed mid-flight**. havoc then installs a cap to a dead region → mmap fails →
   `term.xkb_keymap` stays NULL → `kbd_key` returns early → no char → no typing. This is a real
   cap-transfer use-after-free, latent because only the keymap exercises "sender closes before
   receiver installs" on a single-ref region.
   **Fix (the essential one):** `sys_channel_send` increfs each Shm region whose cap is enqueued;
   `sys_channel_recv` decrefs once after `alloc_slot` installs it. The in-flight ref keeps the
   region alive across the transfer regardless of when the sender closes. (Undelivered caps at
   channel teardown still leak their in-flight ref — a minor, noted follow-up.)

## Verified end-to-end (headless, std-VGA `play` config, QMP key injection + screendump)
Typed `pwd` → echoes, runs, prints `/`. Typed `sleep 30`, hit **Ctrl-C** → prompt returns instantly
(sleep interrupted); `echo hi` → `hi` proves the shell survived. Keyboard AND the §102 PTY/Ctrl-C
subsystem both work. `keymap failures: 0`, keymap fd transfers both ways (`caps=1/1`).

## Files
- `kernel/src/syscall.rs` — `sys_channel_send` incref Shm on enqueue + decref un-enqueued;
  `sys_channel_recv` decref after install. **(The root-cause fix.)**
- `userland/musl-personality/oxbow_syscall.c` — `NR_munmap` (fd-holds-region check), `struct oxfd`
  `seals` field + `fd_alloc_kind` init, `NR_fcntl` `F_ADD_SEALS`/`F_GET_SEALS`.
- `userland/musl-personality/linux_nr.h` — `F_ADD_SEALS`/`F_GET_SEALS`.
- `~/musl-oxbow/havoc/main.c` — `wl_seat` bind version 5 → 7 (out-of-repo).

Note: with fix #4, #2 (seals) and #3 (v7 → cheap path) may be redundant — the refcount fix keeps
the region alive even on the expensive path where `put_fd` closes the copy fd. They're kept because
they're independently correct (proper memfd seal semantics; a fine seat-version bump).

## Key debugging lessons
- **Server→client fd passing was NOT broken** (an early wrong hypothesis) — non-keymap caps
  transferred fine. The keymap fd was lost specifically to `put_fd`'s early close.
- havoc's `fprintf(stderr)` is buffered and never flushes on a stall — use `write(2,...)` for probes.
- The decisive tool was logging channel cap send/recv (`kernel/src/channel.rs try_send/try_recv`)
  and comparing a working boot (munmap reverted, keymap fails, maps) against a broken one
  (keymap succeeds, stalls) op-by-op.
