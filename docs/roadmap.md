# oxbow Master Roadmap — toward a real desktop + Linux/BSD gaming

This supersedes the framing in `linux-desktop-plan.md` (which is still accurate for the
Wayland/Xwayland tracks). It reflects the project's stated endgame: **run real Linux
desktop software, work toward a desktop environment, and eventually port Proton + Wine.**

## Endgame (what we're building toward)
1. Run real, unmodified Linux/BSD **GUI apps** on the oxbow desktop.
2. Reach a **lightweight desktop environment** (XFCE/LXQt-class), not bespoke shell code.
3. **GPU acceleration** (Vulkan via virtio-gpu Venus) — the gate for everything below.
4. **Wine → Proton** — run Windows games (DXVK/VKD3D → Vulkan). The summit.

## Current position (verified rendering on screen)
- Capability microkernel, SMP, virtio-gpu 2D driver, networking, **dynamic linking** (`ld-oxbow`).
- **musl/Linux-syscall personality** runs real upstream apps unmodified.
- **oxcomp** Wayland compositor (real libwayland-server) + GNOME-style shell + graphical login.
- Real Wayland app (**havoc**) renders; native oxui apps (oxterm/sysmon/doom).
- **Xwayland** (real X.org server) runs; **X clients connect over loopback TCP and render**
  (proven: `servers/xclient-musl` maps a window — see `docs/xclient-on-oxbow.png`).
- Transport: loopback TCP + non-blocking sockets + recvmsg/sendmsg on TCP (the X-client plumbing).

## The single biggest lever
**GPU-accelerated Vulkan (virtio-gpu Venus).** It gates the DE polish *and* all of Proton.
Because oxbow runs in a VM, we let the **host GPU** render — no native GPU driver needed.
Everything in Track B converges here.

---

## Track A — Desktop & Apps (build the client side up)
Goal: real X/Wayland apps → window manager → toolkit apps → a DE. Transport is proven.

- **A1. libxcb** — ✅ DONE (commit e933307). xcb-proto codegen + libXau + the 11 core files build
  against musl; `servers/xcbdemo-musl` connects via `xcb_connect("127.0.0.1:0")` and maps a window.
- **A2. libX11 (Xlib)** — ✅ DONE. All 261 core + xcms/xkb/i18n + locale/om/im modules build against
  musl over the libxcb transport; `servers/xlibdemo-musl` does `XOpenDisplay` + XCreateSimpleWindow +
  XMapWindow (cyan window, `docs/libx11-on-oxbow.png`). **← A2 COMPLETE**
- **A3. libXt + libXext + libXmu** (toolkit intrinsics) — ✅ DONE. The whole chain (libXext +
  libICE + libSM + libXt + libXmu) builds against musl; `servers/xeyes-musl` runs the *unmodified
  upstream* xorg **xeyes** (`docs/xeyes-on-oxbow.png`). **← A3 COMPLETE: first real upstream X app**
- **A4. A window manager** — `twm` — ✅ DONE. Graphical login works end-to-end and the post-login X
  session renders: **xeyes draws inside the rootful Xwayland window with twm managing it**, next to
  the native havoc + oxterm windows (`docs/twm-xeyes-on-oxbow.png`). Three fixes got here:
  (1) **event-driven login** — the greeter's verdict read was a blocking `read()` that froze oxcomp's
  whole event loop; now async via `on_session`. (2) **X session spawns post-login**, not at boot.
  (3) **THE blocker — a handle collision**: `BOOT_IMG_TWM` was `52`, the same value as
  `BOOT_SESSION_CHAN=52`. Both install into the shell, so the image handle overwrote the
  session-channel side, closing it; oxcomp's `on_session` then spun on EOF/HUP forever and the shell
  never received the greeter's credentials — login could never complete. (The earlier
  "intermittent kbd input loss" was a misdiagnosis: input was always fine — `root` typed into the
  greeter perfectly; the session channel was just dead.) Fixed by moving the X-demo image handles to
  53–57, clear of the GPU/session data handles.
- **A5. `xterm`** — 🟡 PORTED + BUILDS + RUNS; the in-pty shell works; X-render blocked on one
  fork/socket issue. `servers/xterm-musl` builds xterm-397 (core "fixed" font, **no Xft**) + a new
  **libXaw** port (Athena widgets) on the existing Xt/Xmu/Xext chain. The **PTY subsystem already
  existed** (kernel `pty.rs` + openpty/forkpty in the personality — havoc proves it); A5 added the
  missing pieces around it. Verified working end-to-end: xterm spawns post-login, connects to
  Xwayland over loopback TCP, loads locale, allocates a pty, **forks and execs `/bin/sh` which runs
  in the pty** (`sh:` prompt on serial). Fixes landed (all general personality/kernel improvements):
  (1) `/dev/tty` → ENXIO (no controlling terminal) instead of ENOSYS; (2) `alarm()` + `set*id()`
  no-op success (were ENOSYS, which xterm treats as fatal); (3) **reopenable `/dev/pts/N`** — new
  kernel `SYS_PTY_OPEN_SLAVE` mints a fresh slave cap per open, so openpty apps that close the slave
  and reopen it by name in the child work. **Remaining blocker:** xterm's X connection is a TCP
  (smoltcp) socket; the forkpty child shares it (fork clones the handle table by value) and closes
  it, tearing down the parent's connection → `fatal IO error 11 (EAGAIN)`. havoc survives the same
  fork because its Wayland link is an AF_UNIX channel, not a shared TCP socket. The fix is
  **socket cap refcounting (or CLOEXEC) across fork** — a focused kernel change, tracked next.
- **A6. First real toolkit app** — a single **GTK3** app (Cairo + Pango + Fontconfig + GLib + D-Bus).
  GTK3 over GTK4 (no hard GL requirement for basic widgets). *Milestone:* a GTK window with widgets.
- **A7. Lightweight DE** — **XFCE** (GTK, no mandatory GL) is the realistic DE target, NOT GNOME/KDE.

## Track B — GPU & Gaming (the Vulkan ladder)
Goal: software GL → software Vulkan → accelerated → Wine → Proton.

- **B1. Mesa software OpenGL (llvmpipe)** — port Mesa swrast + LLVM. *Milestone:* a GL demo / GL toolkit.
- **B2. Mesa software Vulkan (lavapipe)** — same gallium base. Self-contained, no kernel 3D yet.
  *Milestone:* **`vkcube` draws on the oxbow desktop** = "Vulkan is alive." (Best gaming on-ramp.)
- **B3. Accelerated GL — virtio-gpu Virgl** — kernel virtio-gpu 3D protocol + Mesa virgl driver +
  DRM render-node ioctls in the personality. *Milestone:* GPU-accelerated GL in the VM.
- **B4. Accelerated Vulkan — virtio-gpu Venus** — venus driver + virglrenderer venus on host.
  *Milestone:* `vkcube`/DXVK-class Vulkan at GPU speed. **This is what Proton needs.**
- **B5. Wine boots** — needs deep POSIX: full pthreads, signals, mmap, large syscall surface,
  and fast sync (`fsync`/`futex_waitv` — already proven valuable in the FreeBSD-Proton work).
  *Milestone:* Wine runs `notepad`.
- **B6. DXVK + VKD3D-Proton + Proton** — sits on B4 + B5. *Milestone:* a game launches.

## Track C — Foundation (cross-cutting, unblocks A & B)
- **C1. POSIX/personality completeness** — the recurring blocker (recvmsg/getpeername were recent
  examples). Each ported app reveals gaps; fix at the personality layer.
- **C2. Dynamic linking maturity** — these stacks are hundreds of `.so`s; harden `ld-oxbow`.
- **C3. PTY subsystem** — real interactivity for terminals (xterm, havoc shell).
- **C4. Audio** — none today; needed for a real "desktop" and for games (ALSA/PipeWire-shaped).
- **C5. D-Bus daemon** — the IPC bus every DE (and parts of Wine) assume.

---

## Sequencing (honest distances)
- **Now → near-term:** Track A1–A5 (real X apps + a WM + xterm). Bounded, builds on proven transport.
- **Parallel/medium:** B1–B2 (software GL/Vulkan) — self-contained; `vkcube` is the morale win.
- **Medium:** A6 (first GTK app) + C5 (D-Bus) — the "real toolkit app" proof.
- **Long:** B3–B4 (accelerated Vulkan), A7 (XFCE).
- **Summit (year-plus):** B5–B6 (Wine → Proton). The hardest item on the roadmap.

GNOME/KDE specifically are deliberately **not** near-term targets: they couple mandatory GPU GL +
a JS/QML engine + the full D-Bus service constellation at once. XFCE (Track A7) is the realistic DE.

## Immediate next step
**A4: a window manager (`twm`).** With the toolkit chain proven through xeyes (A3), the next rung
is a real WM so X windows get decorations/move/resize and the X session feels like a desktop. `twm`
is libXt + libXmu + libXext (all ported) + libXrandr — minimal extra deps. Mirror the per-library
cc::Build pattern in `servers/xeyes-musl/build.rs`. Then A5 = `xterm` (a real terminal X client).
