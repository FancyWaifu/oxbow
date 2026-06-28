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

- **A1. libxcb** — the X11 client protocol library. Needs xcb-proto codegen (host Python)
  + libXau (DONE, builds as a cc group) + pthreads (have). Connect via TCP (DISPLAY=127.0.0.1:0).
  *Milestone:* a minimal xcb client draws a window via libxcb (not raw protocol). **← STARTING HERE**
- **A2. libX11 (Xlib)** on top of libxcb (261 .c files present). *Milestone:* `xev`/a simple Xlib app.
- **A3. libXt + libXext + libXmu/libXaw** (toolkit intrinsics). *Milestone:* `xclock`, `xeyes`.
- **A4. A window manager** — `twm` (tiny, Xlib-only) or `cwm`. *Milestone:* movable, decorated windows.
- **A5. `xterm`** — a real terminal X client (needs the PTY subsystem too).
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
**A1: port libxcb.** Fetch xcb-proto + libxcb, run the protocol generator, build libxcb as a
cc group against musl (mirroring the libXau group in `servers/xwayland-musl/build.rs`), and link a
minimal xcb client that connects over loopback TCP and maps a window — replacing the raw-protocol
`xclient` demo with the real client library every upstream X app uses.
