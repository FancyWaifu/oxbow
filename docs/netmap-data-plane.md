# A netmap-style data plane for oxbow's network path

*Design + staged plan. 2026-06-20. Companion to
`docs/bsd-notes/freebsd-performance.md` (which identified this as the #1 perf lever)
and `docs/freebsd-stack-decision.md`.*

## The problem

oxbow is a microkernel: the app, the net server, and the e1000 driver are separate
processes. A monolithic kernel moves a packet between the socket layer and the NIC
with a **function call**; oxbow moves it with an **IPC domain crossing**. At line rate
(say 1.5 Mpps for small packets) a per-packet IPC is fatal — the domain crossing,
even at ~0.7 µs, caps you at ~1.4 Mpps *before any actual work*. This is *the* thing
that decides whether oxbow's networking is competitive or a toy.

The current path is **per-packet inline IPC**: each `send`/`recv` copies the payload
into/out of a fixed 512-byte message and does one `sys_call`/`sys_send`. Recent work
raised the inline caps to their max (TCP send/recv 504 B, UDP 480 B) and made close a
one-way send — good for small/medium traffic, but it's still *one domain crossing per
packet* and copies the payload twice (app↔server, server↔NIC-DMA).

## The netmap insight (what FreeBSD got right)

netmap (Luigi Rizzo) hits 14.88 Mpps/core by removing both costs at the *kernel↔user*
boundary:
1. **A shared memory region** of preallocated packet buffers, mapped once into both
   the app and the stack. Packets are never copied across the boundary — only buffer
   *indices* move.
2. **Rings of descriptors** (a TX ring and an RX ring) with producer/consumer head
   pointers in the shared region. The app fills slots and advances a pointer.
3. **A batched doorbell**: one syscall (`poll`/`ioctl`) tells the NIC "I've queued N
   packets" or "give me whatever arrived." **One crossing amortizes a whole batch**,
   not one crossing per packet.

For oxbow the boundary to attack is the same shape but it's **server↔server**
(app↔net, net↔driver) instead of kernel↔user. The design transfers directly.

## Where oxbow already is

- The **shared-frame mechanism exists**: DNS already uses `udp::attach` to map
  `NET_SHARED` and `sendv`/`recvv` up to ~1472 B with zero payload copy across the
  boundary. That is netmap Stage 2 for a single buffer.
- The e1000 driver already has **its own DMA RX/TX rings** internally (descriptor
  rings the NIC DMAs into/out of) — the hardware half of the pattern is built.
- Known-missing primitives (noted during the std port): `frame_unmap`, frame *remap*
  (map panics on remap), and **recv exposing the sender PID/badge** — these block a
  clean multi-client shared-ring and are the first things to build.

So oxbow has the *ingredients* (shared frames, NIC DMA rings, a notif/doorbell
primitive) — what's missing is the **ring protocol** that ties them into a batched,
multi-slot, zero-copy data plane.

## The staged plan

### Stage 1 — bigger inline transfers (DONE)
Raise the per-IPC caps to the full message (TCP 504, UDP 480), make socket close a
one-way send. *Effect:* ~9× fewer round trips for small/medium packets; still one
crossing per packet, still two copies. Committed.

### Stage 2 — one shared frame per socket, MTU-sized, zero-copy payload (NEXT)
Generalize the DNS `sendv`/`recvv` shared-frame path to every wire UDP/TCP socket: on
bind/connect, map a per-socket shared frame (a `NET_SHARED`-style region) into both the
app and the net server. `send`/`recv` write/read the payload *in the frame*; the IPC
carries only `(length, offset)` — **no payload copy across the boundary**, and full
MTU (up to ~1500 B) per packet. *Still one crossing per packet*, but the copy is gone
and the size limit is gone (fixes the residual >480 B truncation). **Prereqs:** build
`frame_unmap` + safe frame remap + sender-badge-on-recv (the missing primitives).
*Effort: medium. This is the highest-ROI next step — it's the DNS path, generalized.*

### Stage 3 — a multi-slot ring + batched doorbell (the real netmap)
Replace the single frame with a **ring of N fixed-size slots** + a TX and RX
descriptor ring (head/tail indices) in the shared region, plus a **notification
capability as the doorbell**. The app fills K slots, advances the TX head, and signals
the doorbell *once*; the net server drains K slots per wakeup. Likewise RX: the server
fills slots, signals once, the app drains a batch. **One crossing per *batch*.** Define
the slot-ownership protocol (an owner bit or the head/tail discipline) so neither side
touches a slot the other owns — Rust's type system can enforce a lot of this (a
`Slot<Owned>` vs `Slot<Posted>` typestate). *Effort: high. This is the line-rate lever.*

### Stage 4 — NIC DMA directly into the shared ring (true zero-copy end-to-end)
Today the net server copies between its e1000 DMA rings and the client frame. Stage 4
makes the e1000 DMA *directly* into the shared-ring slots (the app's buffers ARE the
NIC's DMA buffers), so a received packet is touched by the CPU **zero** times between
the wire and the app. This needs the driver to hand DMA-capable buffers from the
shared region to the NIC descriptors. *Effort: high; the last 2× and the thing that
makes "14.88 Mpps" plausible. Do only after Stage 3 proves out.*

## Caveats / what does NOT transfer

- **TCP is a byte stream, not packets** — the ring is natural for UDP/raw frames; for
  smoltcp TCP the win is the zero-copy frame (Stage 2) + batching the segment I/O
  between smoltcp and the driver, not exposing a packet ring to the app.
- **Security**: the shared region is a *capability* shared between exactly the app and
  the net server — it does not weaken isolation (no third party can map it), and the
  ring indices are bounds-checked. This is *better* than netmap's mmap (which trusts
  the userspace not to scribble kernel buffers) because the frame cap names only the
  app's own slots.
- **Don't** copy netmap's `ioctl`/`poll` API shape; use oxbow's notif doorbell + a
  typed ring in Rust.

## Bottom line

Stage 2 (per-socket zero-copy MTU frame) is the right *next* concrete build — it's the
DNS path generalized, fixes the last truncation limit, and removes the payload copy.
Stage 3 (the batched ring) is the line-rate lever and a real project. Together they
are how oxbow's networking goes from "correct and decent" to "competitive," and they
are the *only* honest way to get FreeBSD-class network performance into a microkernel —
by copying netmap's **data-plane design**, not its (or any) C network stack.
