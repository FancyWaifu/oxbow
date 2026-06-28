# Handoff: xterm (A5) → XFCE → GNOME/KDE

**For the next Claude picking up this work. Read this first, then `docs/roadmap.md`.**
Date of handoff: late June 2026. Branch: **`std-native-tls`** (only push here). Commit as
**FancyWaifu** (`git config user.email` should be `77135613+FancyWaifu@users.noreply.github.com`),
trailer `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.

---

## The goal (committed direction)
Reach a real desktop environment: **XFCE first, then GNOME and/or KDE.** The gate ladder is in
`docs/roadmap.md` ("Committed DE plan", gates G0–G6). XFCE is first because it's GTK with no
mandatory GPU GL; GNOME/KDE additionally need GPU Vulkan + full D-Bus + a JS/QML engine.

You are at **G0: make text X clients actually display.** Concretely: get **xterm** to show a
working interactive shell. xterm is ported and runs; the only thing missing is its window content
appearing on screen.

---

## ⚠️ READ THIS so you don't repeat my mistake
I spent ~8 Xwayland rebuild cycles convinced the **builtin font glyphs were broken**. They are NOT.
Proven with server-side probes:
- gunzip decompresses the full 6x13 PCF correctly (`total_out=19584`, exact).
- Glyphs are non-zero at load: `k=0:0x15, k=8:0x1f, k=16:0x3f, k=24:0x3f, k=40:0x0e…`.
- The ONE zero glyph (k=32, offset 1664) is the **SPACE character** (cw=6, no ink) — legitimately
  blank. I sampled that glyph and wrongly concluded the whole font was empty.

**Do not re-investigate the font / pcfread / gunzip / fbglyph paths.** They work.

## What's actually wrong (the real G0 task)
xterm DOES draw its text (proven: 27 `xtermPartString`/`fbImageGlyphBlt` calls into a correct
font), but it never appears. Two intertwined symptoms, both in the **X-presentation path, not
xterm**:
1. **xterm's window mapping is flaky** across builds — a blank white box when spawned alone with
   twm off; absent when xeyes is also spawned; hidden entirely when twm is on (twm does interactive
   placement and never places it).
2. **Incremental (text) damage doesn't reliably reach oxcomp.** The X root background and a
   *continuously animating* client (xeyes — its eyes track the cursor) commit fine and show. A
   one-shot text draw into a late-mapped window does not get composited.

### Where to look
- `servers/xwayland-musl/` glue — the Xwayland→oxcomp present/damage path:
  `xwayland-present.c`, `xwayland-window-buffers.c`, `xwayland-window.c`, `xwayland-shm.c`, plus
  `glue.c`. Question to answer: after an X client damages the rootful screen surface, does Xwayland
  commit the updated buffer to oxcomp, or does it wait for a frame callback that only fires when
  something is animating?
- `servers/oxcomp/src/comp_server.c` — how oxcomp sends `wl_surface` frame callbacks / `done`
  events back to Xwayland. If Xwayland only re-commits on a frame callback and oxcomp only sends
  frame callbacks after a commit, a static client can stall (no commit → no callback → no commit).
- The **twm placement** sub-issue is separate and smaller: twm defaults to interactive window
  placement, so xterm's window never lands with the WM running. Fix = a twm config
  (`RandomPlacement`/`UsePPosition`) or USPosition geometry hints. Don't let it block the commit
  investigation — test with twm off (see harness notes).

### Suggested first step
Add a probe in the Xwayland present/commit glue (log when it commits a buffer to oxcomp + the
damage rect), and in oxcomp's surface-commit/frame-callback handler. Run with twm OFF + xterm only
(see harness) and watch whether a commit happens *after* the shell's text is drawn. If no commit
fires for the one-shot text, that's the bug — make Xwayland commit on damage without waiting for an
animation-driven frame callback.

---

## What IS done and committed (all on `std-native-tls`)
- `43eebc3` — **login fixed**: `BOOT_IMG_TWM` had the same handle value (52) as
  `BOOT_SESSION_CHAN`; the image handle overwrote the session channel in the shell, so graphical
  login could never complete. Moved X-demo image handles to 53–57.
- `2641008` — **xterm + libXaw ported** (`servers/xterm-musl/`). Builds, runs, connects to Xwayland
  over loopback TCP. Also landed general kernel/personality fixes xterm forced out: `/dev/tty`→ENXIO,
  `alarm()`/`set*id()` no-op success, and **reopenable `/dev/pts/N`** (new kernel
  `SYS_PTY_OPEN_SLAVE`, abi=74).
- `cd87f06` — **borrowed socket fds across fork**: a forkpty child closing its inherited X socket was
  sending `TAG_TCP_CLOSE` and killing the parent's connection. Fork child now clears an `owns` flag
  so close drops the cap without teardown. xterm survives forkpty.
- `6ce3a71` — **dup2 stdio fix**: xterm's child used `close(i);dup(ttyfd)`; oxbow's `dup()` only
  returns fds ≥ 3, so the pty slave never landed on 0/1/2 and the shell's I/O went to the console
  (serial) not the pty. Patched the child to `dup2(ttyfd,i)`. **The shell's I/O now reaches xterm.**
- `e877e55`, `b1a7431` — diagnosis commits (b1a7431's font conclusion is the misdiagnosis above; the
  roadmap/task have the correction).
- `abfa2c2` — the committed XFCE→GNOME/KDE roadmap.

**Verified working today:** graphical login (root/root) → twm + xterm spawn post-login → xterm
connects to Xwayland, loads locale, allocates `/dev/pts/0`, **forkpty execs `/bin/sh` which runs in
the pty** (shell prompt confirmed via probe; serial is clean). Everything but the on-screen pixels.

The current task state + corrected diagnosis live in the task list as **task #80** (and #79). The
out-of-repo X sources have NO probes left (all reverted via `git checkout`); the oxbow repo is clean.

---

## Build + test recipe (IMPORTANT — saves you hours)

### Build
- `cd ~/oxbow && just iso` builds everything (kernel + servers + ISO). xterm-specific:
  `cargo build -p xterm-musl`.
- **cargo dir-mtime gotcha:** the `-musl` build.rs files use `rerun-if-changed` on a *directory*
  (`userland/musl-personality`). Cargo only tracks the dir's own mtime, NOT files edited inside it.
  After editing `oxbow_syscall.c`/`oxsys.h`/`linux_nr.h` (or the out-of-repo X sources), **`touch`
  the relevant `build.rs`** (e.g. `touch servers/xwayland-musl/build.rs`) to force a recompile, or
  `cargo clean -p <crate>`. Symptom if you forget: your change silently doesn't take effect.
- The **Xwayland rebuild is the big one** (whole xorg-server). Budget ~2–3 min per cycle. Touching
  `servers/xwayland-musl/build.rs` recompiles only the changed `.c` files + relinks.
- Out-of-repo upstream sources live in `~/musl-oxbow/` (xserver, libXfont2, xterm-397, libXaw-1.0.16,
  libX11, etc.). They are git repos — revert probes with `git checkout <file>`.

### Headless test harness
`/tmp/oxlogin.py` boots oxbow headless and drives login via QMP, then screendumps. Recipe:
- `-vga std -display none -qmp tcp:127.0.0.1:4445` + `-serial file:/tmp/oxserial.log` + the persistent
  disk (`oxbow-disk.img`; create with `just disk` if missing — wiping it is fine, it reseeds).
- It waits for `compositor up`, logs in (warmup `spc`, then `root`/tab/`root`/ret — **password is
  "root"**, the `just play` "root2" note is wrong), then screendumps `/tmp/ox_desktop.ppm`.
- Convert PPM→PNG with `sips -s format png /tmp/ox_desktop.ppm --out /tmp/x.png` then Read the PNG.
- **First boot reseeds the disk (~tens of sec)** — wait for `[fsd] seeded files: N` before expecting
  login. The clock in screenshots is frozen at `00:00:13` (QEMU RTC), NOT a stall — ignore it.
- To SEE xterm's window you must disable twm (it hides it via placement). Temporarily set the twm
  spawn in `servers/oxcomp/src/main.rs` (~line 336) to `if false && spawn_x(...TWM...)`. **Revert it
  before committing.** ErrorF / `write(2,...)` from inside the X server shows up on the serial log.

### Server-side probing
`ErrorF("...")` works inside xserver/libXfont2 code (declare `extern void ErrorF(const char*,...);`)
and lands on the serial log. The `[gz]/[pcflink]/[fbblit]` probe markers I used are all reverted.

---

## Project orientation (oxbow in one paragraph)
oxbow is a from-scratch capability microkernel in Rust (`~/oxbow`), SMP, virtio-gpu 2D, userland
network stack, dynamic linking (`ld-oxbow`). A **musl/Linux-syscall personality**
(`userland/musl-personality/oxbow_syscall.c`) runs unmodified upstream Linux apps. **oxcomp** is a
real-libwayland compositor with a GNOME-style shell + graphical login. **Xwayland** (real xorg-server)
runs as an oxcomp Wayland client; X clients connect to it over **loopback TCP** (smoltcp) and render.
Ported & rendering: havoc (Wayland term), xeyes (graphics — renders), twm, xterm (this task).
No GPU GL/Vulkan, no D-Bus yet — those are later gates (G2/G4/G5).

Good luck. The shell already runs in xterm's pty — you're one X-presentation fix away from a visible
interactive terminal, which unblocks the whole GTK3 → XFCE track.
