# Porting Weston to oxbow — replacing oxcomp with a real Wayland compositor

**Goal:** retire oxcomp's hand-written compositor/shell and run **real upstream Weston**
(libweston) as oxbow's Wayland compositor — "don't reinvent the wheel." Software path only
(oxbow has no GL/EGL); this is also the correct foundation for any later GNOME/KDE attempt
(both mutter and kwin sit on the same lower stack, but additionally require Mesa GL + a big
language runtime + D-Bus/logind — out of scope here, gated on GL).

## Decision: Weston 9.0.0, pixman renderer, fbdev backend

Source: `~/musl-oxbow/weston-9.0.0` (git tag 9.0.0). Chosen because it still ships the
**fbdev backend** (renders via the **pixman software renderer into a linear framebuffer**) —
which is almost exactly oxbow's display model — and has the fewest hard deps (no
libdisplay-info, no color-management, GL/DRM/dbus/systemd/libinput all optional).

Build like every other oxbow C port: a cargo crate + `build.rs` compiling the needed `.c`
via `cc` against a hand-written `config.h` (NOT meson). Precedent: the xserver port compiles
an even larger C tree the same way.

## Reused deps — ALL already ported (validated 2026-07-01)

| libweston needs | oxbow has |
|---|---|
| libwayland-server + wl_shm + event-loop | `servers/oxwl` (wl-src/wl-include) |
| libffi (connection marshalling) | `servers/oxffi` |
| pixman (software renderer) | `~/musl-oxbow/pixman` (already compiled in the xserver build) |
| xkbcommon (keymap) | `servers/oxxkb` |
| drm_fourcc.h (pixel-formats.c) | `~/musl-oxbow/drm` + `linux-headers` |
| musl libc | `~/musl-oxbow/musl-1.2.5` + `userland/musl-personality` |

No new external dependency is required for core + fbdev + pixman.

## The backend contract (reuse oxcomp's plumbing, feed Weston instead)

From `servers/oxcomp/src/main.rs` — oxcomp already proves this model works cross-process:
- **Output:** one linear 32bpp framebuffer. Either the GPU shared fb (`BOOT_GPU_FB` →
  `sys_shm_map(FB_MMIO)`, `GPU_FB_W×GPU_FB_H`, pitch `GPU_FB_W*4`) scanned out by the gpu
  driver, or the Limine fb (`BOOT_FB`). → Weston's fbdev backend renders pixman into this.
- **Input:** two channel fds — keyboard (`BOOT_INPUT_CHAN`) + mouse (`BOOT_MOUSE_CHAN`),
  wrapped by `ox_chan_fd`. oxcomp reads events off them and injects into wl_keyboard/pointer.
  → a small oxbow "input" shim posts `notify_key`/`notify_motion`/`notify_button` into a
  `weston_seat` (bypassing libinput/udev entirely).
- **Clients (inherited-fd model, NOT an AF_UNIX socket):** oxcomp spawns each client handing
  it a channel end as its Wayland socket, then `wl_client_create(display, ox_chan_fd(end))`.
  → keep exactly this: Weston glue calls `wl_client_create` on channel fds. (Weston's usual
  `wl_display_add_socket` is unused — the personality has no named AF_UNIX sockets.)
- **Cursor:** optional GPU hw cursor via `BOOT_GPU_CURSOR` shared region.
- **Greeter/session:** oxcomp's custom login (`BOOT_SESSION_CHAN`) is dropped initially —
  start with Weston's own shell; revisit a session bridge later.

## Minimal libweston file set (core + pixman + fbdev)

INCLUDE (core): compositor.c, input.c, data-device.c, pixman-renderer.c, pixel-formats.c,
plugin-registry.c, bindings.c, clipboard.c, vertex-clipping.c, noop-renderer.c,
linux-dmabuf.c (stub the dmabuf import), log.c + weston-log*.c, plugin bits.
INCLUDE (backend): backend-fbdev/fbdev.c — patched: replace `/dev/fb0` open/mmap/ioctl with
the oxbow mapped `FB_MMIO` + fixed geometry; drop the udev/libinput seat, use the oxbow input
shim.
EXCLUDE (provide stubs/nulls): libinput-device.c, libinput-seat.c (inject input),
launcher-*.c + dbus.c (null launcher), touch-calibration.c, content-protection.c,
linux-explicit-synchronization.c, linux-sync-file.c, screenshooter.c, timeline.c, GL renderer.

## Staged milestones

- **P0 — compile risk (make-or-break, IN PROGRESS):** does libweston core (compositor.c +
  pixman-renderer.c + pixel-formats.c) compile against oxwl/pixman/oxxkb on musl? Surface API
  gaps early (mirrors the havoc port's "biggest risk validated" first step). If oxwl lacks a
  libwayland-server symbol Weston needs, that's the first real blocker.
- **P1 — libweston.a:** the full minimal core + pixman + fbdev links into a static lib, with a
  hand-written config.h and stubs for the excluded pieces.
- **P2 — a compositor process that lights the fb:** a `main()` that inits libweston with the
  fbdev backend pointed at `FB_MMIO`, no clients yet — prove it clears the screen to Weston's
  background. Boot it in place of oxcomp behind a flag.
- **P3 — input:** wire the oxbow keyboard/mouse channels into a weston_seat.
- **P4 — a client renders:** spawn oxterm/havoc handing a channel fd; `wl_client_create`; see
  its surface composited by Weston.
- **P5 — a shell:** bring up weston-desktop-shell (needs cairo + weston client libs) for
  panel/background/window-management, OR a minimal built-in shell first.
- **P6 — swap:** make Weston the default boot compositor; retire oxcomp.

## P0 build recipe (validated — the compile works)

Per-file clang invocation that compiles libweston core (reuse in the P1 build.rs):
- Toolchain: same as xwayland-musl `base()` — `clang -nostdinc -isystem <clang-resource>/include`,
  musl includes (`include`, `obj/include`, `arch/x86_64`, `arch/generic`), `-ffreestanding
  -fno-stack-protector -fno-builtin -Wno-everything -DHAVE_CONFIG_H`.
- **`-D__linux__`** — REQUIRED so `drm.h` takes the `<asm/ioctl.h>`+`<linux/types.h>` branch
  (linux-headers present) instead of the BSD `<sys/ioccom.h>` branch (musl lacks it).
- Include paths: the weston tree (`.`, `include`, `libweston`, `shared`), a scratch dir with
  `config.h`, the **generated protocol headers dir**, `oxwl/wl-include`, `pixman/pixman`,
  `oxxkb/xkb/include`, `drm/include/drm`, `linux-headers`.
- Hand-written generated files: `config.h` (feature macros — musl 1.2.5 has mkostemp/strchrnul/
  posix_fallocate/memfd_create), `git-version.h` (`#define BUILD_ID "..."`),
  `libweston/version.h` (fill `version.h.in`: 9/0/0).
- **Generated protocol headers:** `tools/wl-scanner.py` gained a **`server-header`** mode
  (this session) — emits the request-handler vtable + `wl_resource_post_event` senders + event
  opcodes + `_SINCE_VERSION` (incl. enum-entry) defines. Validated against oxwl's committed
  xdg-shell-server-protocol.h (event-senders 9/9, enums 11/11). Generate `<name>-server-protocol.h`
  + `<name>-protocol.c` (the `.c` is the same private-code as client — wl_interface tables) for
  each protocol a file `#include`s; auto-resolve on demand. Core needs: presentation-time,
  viewporter, xdg-output-unstable-v1, linux-explicit-synchronization-unstable-v1, relative-pointer,
  pointer-constraints, input-timestamps, linux-dmabuf-unstable-v1 (+ weston-* from protocol/).

## dlopen / static modules (P1/P2 runtime concern, NOT a compile blocker)

Weston loads backend/renderer/shell as **dlopen `.so` modules** (`weston_load_module`). musl-static
has no dlopen. Fix at P1/P2: build the modules INTO the binary and replace the module loader with a
static name→`weston_backend_init`/etc. lookup table (the standard static-Weston technique). compositor.c
still compiles (musl ships dlfcn.h).

## P2 progress (2026-07-01) — Weston BOOTS and initializes on oxbow

Wrote the backend + frontend and got real Weston running as an oxbow process:
- `oxbow-backend.c` — a minimal libweston backend modeled on backend-headless (pixman, zero
  udev/libinput/launcher/dlopen). Its output's pixman image wraps `FB_MMIO` DIRECTLY (render ==
  present). Called directly from main (no dlopen); `weston_load_module` is never hit.
- `oxbow-main.c` — a tiny frontend replacing compositor/main.c: maps the fb (native Rust shim
  `oxbow_map_fb`, in `src/main.rs` — tries BOOT_GPU_FB, falls back to BOOT_FB like oxcomp),
  creates the compositor, brings up ONE output via the windowed-output-api + a `simple_head_enable`
  handshake, runs `wl_display_run`.
- **Boot swap:** `WESTON=1 just iso` copies the weston binary over `oxcomp.elf`, so it's spawned as
  "oxcomp" and inherits BOOT_GPU_FB / input / session grants. Plain `just iso` still boots oxcomp.
- **Personality: implemented epoll + timerfd + signalfd/eventfd** (`userland/musl-personality`),
  which libwayland's SERVER event loop needs (weston is the first musl app using it; havoc/Xwayland
  are clients). Factored a shared `fd_revents()` readiness helper (reused by poll + epoll). K_EPOLL/
  K_TIMERFD/K_SIGNALFD/K_EVENTFD fd kinds; a 128-entry epoll registration table; timerfd deadlines
  vs `OX_SYS_UPTIME_MS`; signalfd stubbed (never fires).
- **Bug fixed:** libweston's `default_log_handler` **abort()s** on first `weston_log()` — so
  `weston_log_set_handler()` MUST be installed before any weston call. Added `ox_vlog` (→ stderr).

Serial proof (`WESTON=1`): `[weston] starting → compositor created → backend created → head created
→ output ENABLED on the oxbow framebuffer → entering the event loop`. So real upstream libweston
fully initializes + owns an output on the real framebuffer on oxbow.

**P2 DONE — Weston renders to the oxbow display.** The repaint loop now fires and the screen is a
solid navy fill (the gradient is gone), proving weston's render loop drives FB_MMIO end to end.
The final blocker was a **pre-existing personality bug**: `SYS_UPTIME_MS` returns the value in RDX
(RAX=0 status), but the personality's `ox_syscall0` returns RAX — so EVERY uptime read (clock_gettime
MONOTONIC, the socket recv timeout, and my new timerfd) saw 0. weston armed its ABSTIME repaint timer
at deadline 9ms but uptime stayed 0, so `uptime >= deadline` never held → the timerfd never fired.
Fix: added `ox_uptime_ms()` (reads RDX) and routed all 6 uptime reads through it (`oxsys.h` +
`oxbow_syscall.c`). Also: the backend clears the output to a dark-navy background each repaint (weston
leaves the background to the shell — that's P5); this doubles as the "render loop drives the fb" proof.

## P3 DONE — keyboard + mouse wired into a weston_seat (2026-07-01)

`oxbow-input.c`: creates ONE `weston_seat` with keyboard + pointer, reads the oxbow input
channels, injects via `notify_key`/`notify_motion_absolute`/`notify_button`. No libinput/udev.
- **Input format** (from oxcomp, verified): keyboard channel = raw byte stream, `keycode=b&0x7f`
  (already evdev), `release=b&0x80`; mouse channel = 3-byte PS/2 `[flags,dx,dy]`, left=`flags&1`,
  signs in flags, screen-Y inverted. NO MsgBuf tag. Read the channels via `ox_chan_fd(slot)` +
  plain `read()` (added K_CHAN to the personality's `read()`; it returns raw channel bytes).
- **Keymap**: compile oxbow's self-contained `us_keymap` with `xkb_keymap_new_from_string`. Two
  bugs fixed to get here: (1) `xkb_context_new(XKB_CONTEXT_NO_FLAGS)` errored on the missing
  `/usr/share/X11/xkb` include path and then couldn't compile even a self-contained keymap → use
  `XKB_CONTEXT_NO_DEFAULT_INCLUDES`. (2) weston's `os_create_anonymous_file` (keymap memfd) uses
  `posix_fallocate`, which the personality didn't implement → added **`NR_fallocate`** (sizes a
  K_SHM region like ftruncate). (3) Guard `notify_key` when the seat has no keyboard (else NULL
  deref). Serial proof: `oxbow: input wired — keyboard on, pointer on`, and a key byte was
  observed arriving from the channel. Visible end-to-end verification waits on a client (P4).

## P4 DONE — a real Wayland client renders under Weston (2026-07-01)

A real upstream Wayland client (the `wlclient` "rings" demo) is composited by Weston on oxbow —
verified by screendump: concentric animated rings in a centered 256x256 window over the navy
background. This needed a minimal shell (so P5 is effectively started too):
- **`oxbow-shell.c`** — implements the `weston_desktop_api` on top of **libweston-desktop**
  (weston's real xdg-shell / wl-shell impl): `surface_added` → `weston_desktop_surface_create_view`;
  `committed` (first commit) → center on screen + `weston_layer_entry_insert` + map + schedule
  repaint; `surface_removed` → destroy the view. One `weston_layer` for toplevels.
- **build.rs**: compiles `libweston-desktop/{libweston-desktop,client,seat,surface,wl-shell,
  xdg-shell,xdg-shell-v6}.c` (skips `xwayland.c` — stubbed `weston_desktop_xwayland_init`), plus
  the `xdg-shell` + `xdg-shell-unstable-v6` protocols via the scanner. Include: `libweston-desktop`.
- **Client spawn** (`oxbow_spawn_wl_client` in `src/main.rs`) — the inherited-fd model: channel
  pair, spawn `BOOT_IMG_WLCLIENT` with the client end at slot 4, wrap the server end with
  `ox_chan_fd`, `wl_client_create(display, fd)`. Same pattern oxcomp uses.

Serial: `oxbow-shell: up → client spawned + attached → toplevel added → toplevel mapped 256x256
at 832,412`. So Weston's full client path works on oxbow: wl_compositor + wl_shm + xdg-shell +
seat, cross-process, real upstream libweston-desktop.

**BUILD GOTCHA:** cc-rs sometimes doesn't recompile edited `.c` files → stale binary (cost debug
time twice). `touch servers/weston-musl/*.c` before rebuilding when iterating on the C.

## P5 DONE — multi-window desktop shell (2026-07-01)

`oxbow-shell.c` grew into a real (if minimal) desktop shell: multiple toplevels, tiled layout
(staggered positions so windows don't stack), keyboard focus on activation, a navy background.
`src/main.rs`'s `oxbow_spawn_wl_client(app_id)` now launches several real clients (0=wlclient
rings, 1=havoc terminal, 2=oxterm). Verified by screendump: **two real Wayland clients side by
side under Weston** — havoc (a real upstream terminal, rendering with a cursor) + the animated
rings, on the navy desktop.

**KEY FIX (compositing correctness):** the P2 backend cleared the whole fb to navy EVERY repaint.
weston only re-composites DAMAGED regions, so a static window (havoc) got wiped by the clear and
never redrawn (only the animating rings survived). Fixed: clear the background **once**; weston
then composites windows on top and a static window persists in the fb between its own redraws. (A
proper background is normally a shell surface — this one-time clear is the lightweight stand-in.)
NOTE: havoc renders but prints "could not execute shell" — a havoc-side fs/path issue, not Weston.

## P6 DONE — Weston is the default compositor; oxcomp retired from the boot (2026-07-01)

`justfile`: plain `just iso` now installs the Weston binary as the `oxcomp` boot module (so it
inherits BOOT_GPU_FB + input grants) — **Weston is the default compositor**. `OXCOMP=1 just iso`
rolls back to the legacy hand-written oxcomp (still built for comparison/fallback). Verified:
`just iso` (no env) boots Weston, the shell maps its clients.

Tradeoff noted: the legacy oxcomp had a graphical login greeter + Activities launcher + hardware
cursor + Discord/session bits; the minimal Weston frontend auto-boots straight to the desktop (no
greeter yet). Re-adding a lock/greeter (weston has its own mechanism) and a real panel/background
(the upstream weston-desktop-shell client needs cairo) are follow-ups — the compositor swap itself
is complete.

## THE PORT IS COMPLETE (P0–P6)

Real upstream **Weston runs as oxbow's default compositor**: compiles + links (P0/P1), renders to
the framebuffer via a custom pixman backend (P2), takes keyboard+mouse through a weston_seat (P3),
runs real Wayland clients through libweston-desktop/xdg-shell (P4), does multi-window management as
a shell (P5), and is the default boot compositor (P6). The "don't reinvent the wheel" goal is met.
Along the way this fixed several latent oxbow personality bugs any server-style Linux app would hit:
epoll/timerfd/signalfd/eventfd were missing; `SYS_UPTIME_MS` was read from the wrong register (always
0); `fallocate` and K_CHAN `read()` were unimplemented; plus the shm-region refcount leak.

## Polish — cursor + perf (2026-07-01)

- **Software cursor** (`oxbow-backend.c`): weston tracks the pointer but painted nothing, so the
  mouse was invisible. The backend now draws a small arrow at `oxbow_ptr_x/y` (exported from
  `oxbow-input.c`) each repaint, with save/restore of the pixels under it (no trail). A pointer
  move calls `weston_compositor_schedule_repaint` so the cursor tracks even on an idle desktop.
- **Perf — `use_shadow = true`**: the output was created with `use_shadow=false`, which made the
  pixman renderer repaint the WHOLE 1080p framebuffer in software every frame. With the shadow
  buffer, weston composites + copies only DAMAGED regions to the fb — the main sluggishness fix.
  (Side effect: the desktop background is now the shadow's black rather than the one-time navy
  clear — fine. A real background is a shell surface, a follow-up.)
- **Remaining perf ceiling (not yet addressed):** oxbow's event loops are cooperative busy-*yield*
  loops (`epoll_wait`/`poll`/`select` spin with `SYS_YIELD` instead of truly blocking), so weston
  AND every client spin-wait and compete for CPU. A real fix = blocking waits with kernel wakeups
  (wake a process when a watched fd/timer becomes ready) — a meaningful kernel change, deferred.

## Responsiveness — blocking waits instead of busy-yield (2026-07-01)

The big one. The personality's `poll`/`select`/`epoll_wait` were cooperative busy-*yield* loops
(spin + `SYS_YIELD`), so weston AND every client pinned a core at 100% — which **starved the other
boot servers**. Concretely: on a FRESH disk, fsd never got enough CPU to seed the ext2 image, so
`/bin/sh` (havoc) and `/lib/liboxui.so` (wlclient) were missing → clients couldn't run → **black
screen + no seeding** (exactly the reported symptom).

Fix: route those loops through the kernel's existing **`SYS_CHAN_WAIT`** (which already does proper
park/block/wake with a timer deadline) instead of spinning. New personality helper `block_wait_fds`
gathers the watched fds' channel handles + nearest timerfd deadline and truly SLEEPS on
`ox_chan_wait(chans, n, timeout)` until a channel is readable or the deadline fires. Wired into
`NR_epoll_wait`, `NR_poll`/`NR_ppoll` (rewritten to loop+block; parses the poll/ppoll timeout), and
`NR_select`. Safety: falls back to a single YIELD when there are no channels to sleep on (socket-only
selects like darkhttpd — no regression) and caps the wait to 8 ms when sockets/ptys are present (so
they still get polled). Also hardened `fd_revents` to treat untracked std fds (0/1/2) as ready, matching
the old poll (else a shell polling stdin would hang).

**Verified (fresh disk):** fsd seeds in **~4.5 s** (was starved), the rings client maps at ~6 s, havoc's
shell runs (`#` prompt), the mouse cursor is visible — no more black screen. No busy-spin. No regressions
(clients + fsd + boot all fine).

## Perf round 2 — idle desktop + GPU sleep + real blocking waits (2026-07-01)

Three more fixes after the user reported it was still laggy (and the screen black / terminal not
seeding):
1. **Blocking waits everywhere** — the personality's `poll`/`select`/`epoll_wait` now truly SLEEP
   on the kernel's `SYS_CHAN_WAIT` (via `block_wait_fds`) instead of busy-yielding. This was the
   big one: the busy-spin pinned cores and STARVED fsd — on a fresh disk fsd couldn't seed `/bin`
   + `/lib`, so the clients couldn't run → black screen + "no seeding". After: fsd seeds in ~4.5s,
   clients render. (Falls back to yield for socket-only apps; caps at 8ms when sockets/ptys present.)
2. **Idle desktop** — default-spawn ONE idle client (havoc terminal) instead of the rings demo,
   which animated every frame and kept the compositor busy nonstop (oxcomp defaulted to an idle
   terminal too). Now weston sleeps between events and only repaints on interaction.
3. **GPU `SYS_SLEEP`** — added a real sleep syscall (`SYS_SLEEP=53`, kernel `sys_sleep` using the
   proven `set_wake_at`+block+timer-wake path) and replaced the gpu present loop's busy-spin
   `frame_delay` (`for 0..2_000_000 spin_loop()` — "no sleep syscall yet") with `rt::sys_sleep(16)`.
   The gpu no longer pins a whole core just to pace its ~60fps whole-fb scanout. Verified with a
   real virtio-gpu: the present loop heartbeats fine (sys_sleep doesn't hang) and weston maps its
   client on the gpu path. `SYS_SLEEP` is a general primitive any native server can use to stop
   busy-waiting.

## Status / log

- 2026-07-01: survey done. All core deps present; Weston 9.0.0 cloned; plan locked. Backend
  contract extracted from oxcomp.
- 2026-07-01: **P0 DONE — libweston core compiles on oxbow.** Compiled to .o against the oxbow
  stack: compositor.c (121 KB obj), input.c, data-device.c, pixman-renderer.c, pixel-formats.c,
  linux-dmabuf.c. NO source-portability blockers hit — only build scaffolding (protocol headers,
  version stubs, `-D__linux__`). Added `server-header` mode to `tools/wl-scanner.py` (reusable).
- 2026-07-01: **P1 DONE — libweston.a builds + links on oxbow.** New crate `servers/weston-musl`
  (registered in the workspace). `build.rs` compiles, via the xwayland-musl toolchain pattern, five
  groups: pixman, libwayland(server+client)+ffi, xkbcommon, **libweston core** (all 24 srcs_libweston
  + 3 shared: matrix/os-compatibility/xalloc + 16 generated protocol `.c`), personality+glue. It
  generates version.h (from `include/libweston/version.h.in`) and all protocol server-headers/code
  via the scanner at build time. `liboxweston.a` = 43 objects; `weston_compositor_create` /
  `weston_output_init` / `weston_surface_create` / `weston_seat_init` / `pixman_renderer_init` all
  defined. Links into a 3.6 MB static `weston` ELF (stub C `main` in `glue.c` for now). Build fixes:
  `version.h.in` is under `include/libweston/`; added a `shim/linux/ioctl.h` (`#include <asm/ioctl.h>`)
  since musl-oxbow's linux-headers lacks that wrapper (linux-sync-file-uapi.h needs it).
  NOTE: fbdev backend NOT built yet — it's the backend/main wiring, moved to P2.
  Next: P2 — patch backend-fbdev/fbdev.c to render pixman into oxbow's FB_MMIO, add the static
  module loader (no dlopen), and a real `main()` that lights the screen.
