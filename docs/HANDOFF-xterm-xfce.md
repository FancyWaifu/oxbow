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

## ✅ SOLVED — G0 done: xterm renders its interactive shell (2026-06-28/29 session)

**xterm now displays the `/bin/sh` banner + prompt on screen** (verified by screendump,
reliably across runs). The one-line fix is in xterm's `in_put()`.

### Root cause
xterm draws fine, but its text never reached the X server. The chain (all probes reverted):
1. The **font is fine** (don't chase it). 6x13 loads with non-zero bitmaps; `fbPolyGlyphBlt`
   renders real glyphs with ink. The earlier "empty glyph" evidence was the SPACE character
   from the screen-clear (PCF offset 1664 is legitimately blank) — a red herring that
   misdirected both the prior session AND a long stretch of this one.
2. The shell banner reaches xterm (`readPtyData` returns 51 bytes), and xterm draws it
   (`dotext`→`WriteText`, `AddToVisible`=true, `xtermPartString`→`XDrawImageString`).
3. **But the request never hit the wire.** xterm is built with `USE_DOUBLE_BUFFER=0`
   (`xtermcfg.h` has no XDBE), so `xtermFlushDbe()` — its flush-before-block point — is a
   **no-op** (`#define xtermFlushDbe(xw) /* nothing */`, xterm.h:1400). xterm's main loop
   `in_put()` draws the banner, then calls `readPtyData()` → `read()` on the **pty master**,
   which **blocks** (oxbow pty read is blocking; shell is idle at the prompt). So xterm
   sleeps in the read with the banner sitting unsent in the Xlib output buffer, and never
   reaches any flush. The init-time screen-clear showed because startup `XSync` flushed it.

### The fix (committed)
`servers/xterm-musl` → `~/musl-oxbow/xterm-397/charproc.c`, top of `in_put()`'s `for(;;)`:
add `XFlush(screen->display);` **before** the `readPtyData()` that may block. Now whatever
was drawn in the previous pass is flushed before xterm sleeps on the pty. One line + comment.

### Dead-ends tried (so the next person doesn't repeat them)
- Making the pty **non-blocking** (honor `O_NONBLOCK`/`FIONBIO` in the personality + kernel
  `sys_pty_read`): works mechanically, but exposes that the personality's `NR_select` reports
  **every fd always-ready** (only K_LISTEN is checked; K_SOCK has no readiness probe at all),
  so xterm busy-loops and starves the shell. The blocking-read-as-yield is load-bearing in
  the current design. All those kernel/personality changes were reverted. **If you ever make
  ptys non-blocking, you must first give `NR_select` real readiness checks (mirror `NR_poll`:
  K_CHAN via `ox_chan_poll`, K_PTY via `ox_pty_ioctl(…,0x100)`) AND add a K_SOCK readiness
  primitive — neither exists today.**
- Window-buffers single-buffering, DBE force-off in xterm, present/damage probing — all
  irrelevant (reverted). Present/commit/frame-callback/child-damage paths are fine.

## ✅ SOLVED — G1 core: twm now manages, frames, and places X clients (2026-07-01 session)

**twm adopts xterm, builds a decoration frame, reparents+places it, and the framed window
renders on the X screen** (verified by screendump). twm is now spawned in
`servers/oxcomp/src/main.rs`. The blocker below ("twm never receives the MapRequest") is fixed.

### Root cause — loopback socket readiness was never reported to select/poll
twm's libXt was built **without `USE_POLL`** (`~/musl-oxbow/libXt-1.3.0/src/config.h` has no
`USE_POLL`), so `XtAppNextEvent` waits in **`select()`**, not `poll()`. The personality's
`NR_select` reported **every fd always-ready** (only K_LISTEN was gated). So twm's select
returned "X fd readable" immediately, twm did a **blocking `recv`**, and that recv **pins the
single-threaded net server for up to 8 s** — during which the X server's `TAG_TCP_SEND` of the
redirected MapRequest can't be processed. Classic loopback deadlock: the receiver's blocking
recv starves the sender whose send would produce the data. (Server-side was proven correct:
`MapWindow`→`MaybeDeliverMapRequest` delivers the MapRequest to twm and `FlushClient` fully
writes it — it just never got drained.)

### The fix (landed) — non-consuming socket readiness peek, gate select/poll on it
Four layers (mirrors the existing K_CHAN/K_PTY readiness gates):
1. `servers/net/src/tcp.rs` — `recv_ready(handle)`: poll once, return `can_recv() || !may_recv()`
   without consuming.
2. `servers/net/src/main.rs` — `TAG_TCP_RECV` gains a **peek** mode (`data[1]==2`): reply
   `data[0]=1/0`, no bytes consumed.
3. `rt/src/lib.rs` — `tcp::recv_ready` + hosted shim `__oxbow_sock_recv_ready(sock)->i32`.
4. `userland/musl-personality/oxbow_syscall.c` — in **both** `NR_select` and `NR_poll`, a
   `K_SOCK` fd reports readable only when `__oxbow_sock_recv_ready()` is true (decl in `oxsys.h`).
Now twm blocks in select's **yield-loop** (a quick peek IPC per iteration, not an 8 s recv pin),
so the net server stays free to process the X server's send; when the MapRequest lands, the peek
flips ready, twm reads it, `HandleMapRequest`→`AddWindow` runs, and the client is framed+placed.
This is a general transport fix — every X client / WM interaction over loopback benefits. The
earlier session's `writev` partial-send fix (below) is still needed and still landed.

### STILL OPEN (next step, G1 polish) — the framed client body is blank white
twm frames+places xterm, but xterm's **content area is blank** (only the twm titlebar draws).
Probing (reverted) showed **xterm receives MapNotify + PropertyNotify but no `Expose` and no
`ConfigureNotify`**, so it never repaints its VT content. Cause is an **Xwayland/dix nested-
exposure** issue, not transport: in `HandleMapRequest` twm does `XMapWindow(client)` **before**
`XMapWindow(frame)`, so when the client window is mapped its parent (the frame) isn't realized
yet — `MapWindow` hits `if (!pParent->realized) return Success;` and sends no Expose. When the
frame is then mapped and `RealizeTree` realizes the whole subtree, the server does **not** emit
an Expose for the now-viewable nested client window. Next: in `~/musl-oxbow/xserver/dix/window.c`
`MapWindow`/`RealizeTree` (or the screen `HandleExposures`/`miWindowExposures` path), make
realizing a subtree via a parent's map generate Expose for the newly-viewable descendant windows
(what a stock X server does). Verify: framed xterm shows its `/bin/sh` banner+prompt.
Separately (cosmetic): twm emits ~22 `BadName`/`BadFont` X errors for its default named fonts
(`variable`) and named colors — harmless (falls back to `fixed`/mono), but a real font-path +
RGB color database would silence them.

## ✅ SOLVED — wl_shm region leak that crashed the X server (2026-07-01 session)

**Symptom (from a longer real run):** after twm+xterm come up, the serial shows ~15
`wl_shm_create_pool` successes, then repeated `os_create_anonymous_file done` →
`anon file FAILED (fd<0)`, then `X connection to 127.0.0.1:0 broken` + `dix_main returned` —
Xwayland dies. The headless 130 s screendumps just beat the crash.

### Root cause — Shm regions were never freed (3 missing layers + no refcount)
Xwayland sets `pScreen->CreatePixmap = xwl_shm_create_pixmap` (`xwayland-screen.c:1206`), so
**every X pixmap** allocates a kernel shm region (`NREGIONS = 16`, `kernel/src/shm.rs`). Pixmaps
are destroyed constantly (`xwl_shm_destroy_pixmap` → `munmap`), which should recycle the region,
but the free path was missing everywhere: personality `NR_munmap` was `return 0;` (no-op);
`fd_release` had no `K_SHM` branch; kernel `close_handle`(Shm) did nothing and `shm::free` was
**never called**; and there was **no refcount**. So ~15 pixmaps exhausted the 16-slot pool →
`ox_shm_create` fails → Xwayland dies.

### The fix (landed) — reference-count Shm regions by handle-table entries
Verified first that SCM_RIGHTS fd-passing is **grant-by-COPY** (`sys_channel_send` copies the
HandleEntry, sender keeps its cap; `sys_channel_recv` installs a *new* entry) — so oxcomp and
Xwayland each hold an independent handle to the same `Shm(idx)`. Correct refcount = **# handle
entries → `Shm(idx)`**:
- **Kernel** (`shm.rs`): `Region` gains `rc` + `mem_idx`. `incref` at the two raw store sites
  (`Process::alloc_slot` + `install`, `proc.rs`) — this is the airtight choke point (missing an
  incref = UAF; missing a decref = mere leak). `decref` at every removal (`Process::close`,
  `close_handle`, `close_all`); at `rc==0` it frees the frames and refunds `mem_idx`.
  `sys_shm_create` records `mem_idx` and frees the orphan region if no handle can be installed.
- **Personality** (`oxbow_syscall.c`): an `mmap` of a `K_SHM` region owns a reference for as long
  as the MAPPING lives (wl_shm closes the memfd right after mmap but keeps rendering). So we track
  `(va → shm cap)` in `g_shm_maps`, `close(fd)` does NOT drop the cap while a mapping owns it, and
  `munmap` closes it (`SYS_CLOSE` → kernel decref). The region frees only once BOTH Xwayland (at
  munmap) and oxcomp (libwayland pool destroy → munmap+close) release. Double-close is safe
  (`close_handle` returns BadHandle with no decref on an empty slot).

**Verified:** the crash is gone (0 `anon file FAILED` / `connection broken` / `dix_main returned`
across a full run), the desktop still renders (the boot framebuffer Shm is `install`-incref'd and
long-lived, so never wrongly freed), no panics. **Standing regression:** jail's `[41] shm region
recycles on close` — creates+closes a 1-page region 32× (2× the pool); a leak fails at #17 with
E_NOMEM. Run the jail suite to exercise it (needs a shell; not wired into the desktop-boot path).

**Known limitation (follow-up hardening):** freed frames are NOT unmapped from the mapper's page
tables (no `vm::unmap_user_4k_live` exists yet). Benign today because oxbow uses bump VA allocators
(no address reuse → stale PTEs never alias live data) and mappers don't touch destroyed-buffer vas.
Full cross-AS unmap = a later isolation-hardening item.

### G1 — bring up the window manager (original blocker analysis, now resolved above)
twm was **not** started in `servers/oxcomp/src/main.rs` (deferred), because with it on, xterm
was invisible. Probing twm (all probes reverted) pinned the blocker precisely:

- twm **does** start and acquire the WM role — no "another window manager running" error, it
  builds its icon manager, and SubstructureRedirect **works**: twm receives a redirected
  `ConfigureRequest` (event type 23). So the redirect machinery is functional.
- But twm **never receives a `MapRequest` (type 20) for xterm**, and `AddWindow` never runs
  for it (only for twm's own "TWM Icon Manager"). So xterm is never reparented/decorated/placed
  → it stays unmapped (invisible) while twm holds the redirect.
- twm's whole event stream for the run was just: `ConfigureRequest(0x6d)`, then
  `ReparentNotify`/`ConfigureNotify`/`PropertyNotify` for xterm's window `0x400023` — and then
  silence. No map ever flows.

**UPDATE — the X side is NOT the problem; it's loopback-TCP event delivery.** Server-side
probing (all reverted) proved the redirect path is fully correct:
- xterm's toplevel maps as a child of root with `mapped=0, overrideRedirect=0`, and the parent
  (root) `RedirectSend` = true. `MapWindow` → `MaybeDeliverMapRequest` → `result=1` (delivered
  to exactly one client = twm). Root's *own* eventMask doesn't have SRM, so it correctly routes
  to twm's OtherClient entry, not the server.
- The server then **fully writes** that MapRequest to twm's socket: `FlushClient` for twm fires
  with `wrote == notWritten` (no partial writes). The event leaves the server.
- **But twm never receives it.** twm's `XtAppNextEvent` returns ~4 events
  (`ConfigureRequest`/`ReparentNotify`/`ConfigureNotify`/`PropertyNotify`) and then goes
  **silent** — type 20 (MapRequest) never surfaces — so `HandleMapRequest`/`AddWindow` never run.

So the blocker is **server→twm delivery of the redirected event over oxbow's loopback TCP**:
the server writes it, twm never receives it.

**ONE ROOT CAUSE FOUND + FIXED: `writev` dropped bytes on a short socket send.** The personality's
`NR_writev` (`userland/musl-personality/oxbow_syscall.c`) looped over the iovecs calling
`do_write` per iovec, and on a **partial** socket send (smoltcp's TX buffer can accept fewer
bytes than asked — `tcp_stack.send` returns < len) it added the partial count and **continued to
the next iovec**, dropping the unsent tail and putting a GAP in the X byte stream. `FlushClient`
writes 3 iovecs (buffered output + new event + pad), so when twm's socket buffer was partly full
the stream desynced — xcb mis-parsed everything after the gap (this is also where the **10
spurious X errors** twm saw came from). **Fix (landed):** POSIX `writev` semantics — stop at the
first short write and return the count so the caller retries the remainder. Verified: it measurably
improved event flow to twm (15 PropertyNotify vs 1; a MapNotify now arrives) and **does not break
G0** (xterm still renders). This is a real transport-correctness bug independent of the WM.

**RESOLVED (2026-07-01):** the "twm stops after ~20 events, MapRequest never arrives" stall was
the **select-always-ready → blocking-recv pins the net server** deadlock, now fixed by the socket
readiness peek (see "✅ SOLVED — G1 core" above). twm now receives the MapRequest, manages+frames+
**places** xterm. Placement works (twm centers it; USPosition honoring is a further tweak if
wanted). The only remaining visible gap is the blank client body (nested-Expose issue, above).

---

## ⚠️ (OBSOLETE) READ THIS so you don't repeat my mistake
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
