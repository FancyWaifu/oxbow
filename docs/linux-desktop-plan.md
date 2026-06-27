# Blueprint: run a real Linux/BSD desktop environment on oxbow

## The thesis (why this is achievable, not a fantasy)

oxbow already runs **havoc** — a real, unmodified upstream Wayland GUI terminal — on
its own compositor (**oxcomp**, running real `libwayland-server`) via the **musl
personality**, with an interactive `/bin/sh` inside it. That means the genuinely hard,
novel part is **already done and proven**: a real Wayland client stack runs on a
from-scratch capability microkernel.

A "Linux desktop environment" is, mechanically, just **more Wayland clients + a window
manager**. So we stop hand-building oxbow's own shell and instead **port the ONE
foundation that every modern Linux desktop is built on**, after which DEs and apps
become a *build target*, not bespoke code.

That foundation is **wlroots**. sway, Hyprland, labwc, river, Wayfire, cage — every
modern non-GNOME/KDE Wayland desktop — are thin programs on top of wlroots. Port
wlroots once → get the whole family.

## Two tracks, one foundation (we have Wayland — so add X11 too)

oxbow already has a working Wayland compositor (oxcomp) + a real Wayland client
(havoc). So we pursue **two desktop tracks that share one foundation** and complement
each other on the same screen:

- **Track X (X11 — do this for the classic desktop + the whole legacy app catalogue):**
  **Xwayland → twm/openbox → xterm + X apps → XFCE.** Xwayland is an X server that runs
  *as a Wayland client of oxcomp* — architecturally it's "havoc but bigger": a real
  upstream Wayland-client C app. Its dispatch loop is **`poll`-based** (xserver `ospoll`
  poll backend), so — unlike wlroots — **it does NOT need the epoll shim**; it rides the
  exact path havoc already proved. This is the shorter road to "a recognizable Linux
  desktop" and it unlocks every X11 app + classic DE (XFCE, openbox, fvwm, twm).
- **Track W (modern Wayland DE):** **wlroots (nested) → tinywl → sway / labwc.** The
  modern, tiling, pure-Wayland desktop family. Bigger foundation work (needs
  wayland-server event loop = the epoll shim) but no X protocol emulation.

Both tracks share: **pixman** (software rendering — port once, both use it), oxcomp
advertising a little more (`wl_output`), and the proven wayland-client transport. X apps
(Track X) and Wayland apps (Track W) end up as windows on the *same* oxcomp desktop.

**Recommended order given "we already have Wayland": do Track X first** (Xwayland unlocks
the most, reuses havoc's path, skips epoll), then Track W in parallel/after.

### Track W backend choice (when we get there)

**wlroots (nested) → tinywl → sway / labwc**, with this exact backend choice that
sidesteps every hardware/driver problem:

- **Wayland backend** (not DRM/KMS): wlroots runs as a *client of oxcomp*. It uses
  oxcomp's output and input (wl_seat) instead of touching DRM/KMS/libinput. oxbow's
  Wayland transport is the proven path (havoc).
- **Pixman software renderer** (not GLES/EGL/GBM): composite in software into `wl_shm`
  buffers. No GPU/EGL/dmabuf needed. (virtio-gpu accel is a *later* optimization.)
- **noop session** (not logind/seatd): the Wayland backend borrows the parent's seat,
  so no session/seat management is required.

This reduces wlroots' enormous dependency surface to: **wayland-client/server (HAVE),
pixman (port), xkbcommon (HAVE), + wlroots core**. Nothing else.

### Alternatives (documented, not chosen first)

- **X11 via Xwayland → openbox / XFCE / fvwm.** Unlocks the *entire legacy X11 app +
  classic-desktop world*; classic WMs are tiny (Xlib only). But porting Xwayland = the
  X.org server core, comparable effort to wlroots, and X is a dead-end protocol. Good as
  a *second* branch (Phase 5) once wlroots works, to also run X apps.
- **GNOME Shell / KDE Plasma.** The hardest target: full GTK4/Qt6 + GLib/GObject + a
  live D-Bus session bus + a stack of services (gsettings/dconf, polkit, portals,
  pipewire). Deferred indefinitely — this is the "boil the ocean" path. (sway needs
  almost none of this; that's why it's the spine.)

## Current state vs. gaps (grounded in the tree, 2026-06)

HAVE (proven): capability microkernel + SMP; oxcomp running real libwayland-**server**
(`servers/oxwl/wl-src/{wayland-server,event-loop}.c`); musl personality runs real
upstream C apps; AF_UNIX/SCM_RIGHTS + wl_shm transport; kernel **pty**; **FreeType**;
fork/exec/wait, sockets, dynamic linking; `tools/wl-scanner.py` (protocol codegen).
oxcomp advertises **wl_compositor, wl_seat, xdg_wm_base**.

GAPS to close (the whole job, ranked):
1. **epoll / eventfd / timerfd in the musl personality** — ZERO today. Every
   `wl_event_loop` (libwayland-server) + wlroots needs them. **Do NOT add kernel epoll**
   — implement them as a **userland shim over the personality's existing `poll`**
   (the FreeBSD/macOS `epoll-shim` technique). eventfd = a pipe-backed counter; timerfd =
   a poll-timeout + monotonic clock. Bounded, self-contained.
2. **oxcomp must advertise `wl_output`** (wlroots' wayland backend needs output
   geometry) and verify `wl_shm`, `wl_subcompositor`, and that `wl_seat` exposes
   keyboard+pointer capabilities the backend expects.
3. **pixman** — port (pure, portable C; compiles like havoc's deps against musl).
4. **wlroots core** — build against musl via `build.rs` (compile sources directly, like
   havoc; generate protocol headers with `wl-scanner.py`), with only the wayland backend
   + pixman renderer + noop session compiled in.
5. **json-c + pcre2** — sway's deps (small, portable). labwc needs cairo+pango+libxml2
   instead — heavier; sway is the lighter first DE.

## Track X (X11 via Xwayland) — the phase ladder (recommended first)

Xwayland = the xserver tree built as **Xwayland DDX only, software (`fb`) path, no
glamor** (→ no libdrm/EGL/GBM), **ospoll poll backend** (→ no epoll). Deps: pixman
(shared), xkbcommon (HAVE), wayland-client (HAVE), a font source (built-in fixed font
to start — defer libXfont2/fontconfig). Source out-of-repo at `~/musl-oxbow/xserver`.

- **X0 — pixman** (shared foundation). New `servers/oxpixman/` (mirrors `servers/oxwl`):
  vendor/clone pixman, compile against musl via `build.rs`, expose a static lib. Smoke:
  a tiny program links pixman and fills + reads back a pixman image. *Bounded, no
  X knowledge needed — the right first brick.*
- **X1 — Xwayland comes up.** Build Xwayland (poll backend, fb, no glamor) as a musl app
  (`servers/xwayland-musl`, like `havoc-musl`); generate the X protocol + version headers;
  stub the Linux-isms (it's poll-based so this is small). Launch it as an oxcomp client.
  Milestone: `Xwayland :0` initializes, creates a root window/screen, presents a (blank)
  surface to oxcomp. **Rootful/`-fullscreen` mode first** — one big X screen inside a
  single oxcomp window (the lazy, self-contained target; no rootless integration yet).
- **X2 — first X client renders.** Port minimal Xlib stack (libxcb + libXau + libX11; for
  GUI: libXext). Run a tiny Xlib-only app — `xclock`/`xeyes`/`xlogo` — against `:0`.
  Milestone: a real X11 app draws inside the Xwayland window on oxbow.
- **X3 — a window manager.** Port **twm** (Xlib + libXmu + libXt — small, in the X tree).
  Milestone: X windows get titlebars/move/focus — a managed classic X session in a window.
- **X4 — a terminal + the classic desktop.** `xterm` (libXt + libXaw) or a lighter X
  terminal; then **openbox + tint2** (or fvwm) for a real classic desktop. XFCE later
  (needs GTK2/3 → GLib/cairo/pango/gdk-pixbuf — a heavier sub-port, but all portable C).
- **X5 — rootless integration (polish).** Switch Xwayland to rootless so each X window is a
  native oxcomp window beside Wayland apps (needs a cooperating WM; in Track-W this is
  sway's `xwayland` support; standalone, a small shim WM). Optional.

## Track W (wlroots/sway) — the phase ladder (modern Wayland DE; after/parallel to Track X)

### Phase 0 — Close the foundation gaps  ·  milestone: a libwayland-server program runs nested
- 0a. **epoll/eventfd/timerfd shim** in `userland/musl-personality/oxbow_syscall.c`
  (poll-backed). Add the NRs to `linux_nr.h`. Level-triggered, EPOLLIN/EPOLLOUT — enough
  for `wl_event_loop`.
- 0b. **oxcomp: advertise `wl_output`** (+ verify wl_shm/subcompositor) so a nested
  Wayland-backend client sees an output.
- 0c. **Port pixman** as a musl static lib (new `servers/oxpixman/` mirroring `oxwl`).
- **Smoke test (de-risk the scary unknown FIRST):** a ~50-line musl program that creates
  a `wl_event_loop`, connects to oxcomp as a Wayland client, and pumps the loop. If it
  runs, the epoll shim + nesting are proven before touching wlroots.

### Phase 1 — wlroots builds + tinywl runs  ·  milestone: first real Linux compositor on oxbow
- Build wlroots (musl, `build.rs`) with: `-Dbackends=wayland -Drenderers=pixman
  -Dxwayland=disabled -Dsession=disabled -Dexamples=disabled`, headers via wl-scanner.
- Run **tinywl** (wlroots' ~1000-line reference compositor) as an oxcomp client.
- Stub the Linux-isms wlroots' core still pulls in (signalfd → ignore; any leftover DRM
  headers → provide empty shims). The wayland backend avoids the big ones.

### Phase 2 — A client inside the ported WM  ·  milestone: nested compositing works
- Run **havoc** (already ported) INSIDE tinywl: oxcomp → tinywl → havoc.
- Verify focus / move / a second window. Proves the ported compositor actually composites
  real clients, not just clears a screen.

### Phase 3 — A real desktop environment  ·  milestone: sway running on oxbow
- Port **sway** (deps: json-c, pcre2; **defer swaybar** = cairo+pango at first).
- Ship a `sway` config: tiling, keybindings, workspaces. This is a genuine daily-driver
  Linux Wayland DE running on the microkernel.
- (Alternative: **labwc** for a classic stacking/openbox-style desktop — costs cairo+pango
  +libxml2 up front.)

### Phase 4 — Desktop furniture  ·  milestone: a *usable* desktop
- **Bar**: swaybar/waybar (port cairo+pango — the GTK-less text/vector stack) OR a
  lighter bar (`yambar`/`somebar`).
- **Launcher**: `bemenu` / `fuzzel` / `wofi`.
- **Apps**: `foot` (terminal), an image viewer, a file manager. The terminal already
  works (havoc), so a shell + tools are there day one.

### Phase 5 (optional) — X11 bridge  ·  milestone: legacy X apps in the Wayland desktop
- Port **Xwayland** → run `xterm`, `xclock`, classic X apps, and X-only software inside
  the wlroots desktop. Unlocks the entire X11 back-catalogue without a separate X server
  on bare metal.

## Honest risk register
- **wlroots Linux-isms beyond epoll**: signalfd (stub/ignore — only used for signal
  handling we can do via the personality), timerfd (Phase 0a covers it), leftover DRM/GBM
  headers even with those backends off (provide empty shims). The wayland+pixman+noop
  combination is specifically chosen because it dodges DRM/KMS/libinput/EGL entirely.
- **Build system**: wlroots/sway use meson; we compile sources directly via `build.rs`
  like havoc. Tedious (enumerate sources, generate protocol + version headers) but a
  proven, mechanical pattern here.
- **D-Bus**: sway runs without a session bus (some niceties degrade); GNOME/KDE do not —
  another reason sway is the spine and GNOME/KDE are deferred.
- **Performance**: software (pixman) compositing of a nested compositor will be slow at
  1080p. Fine for proving it; virtio-gpu-accelerated rendering is a later, separate lever.

## Why this ends the fatigue
After Phase 1 (wlroots builds), new desktops and apps stop being *oxbow code* and become
*upstream software we compile*: sway, labwc, river, foot, fuzzel, mako, grim — all
wlroots/Wayland clients that build the same way havoc did. The reinvention stops; porting
begins.
