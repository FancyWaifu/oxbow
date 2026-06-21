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
  (map panics on remap), and **recv exposing the sender PID/badge**. Stage 2 (UDP)
  turned out NOT to need them — per-socket frames key off the already-per-socket badged
  socket cap, and the cap-copy + fixed per-sid binding mean each frame is mapped exactly
  once and kept (no unmap/remap). A multi-slot ring (Stage 3) may still want them.

So oxbow has the *ingredients* (shared frames, NIC DMA rings, a notif/doorbell
primitive) — what's missing is the **ring protocol** that ties them into a batched,
multi-slot, zero-copy data plane.

## The staged plan

### Stage 1 — bigger inline transfers (DONE)
Raise the per-IPC caps to the full message (TCP 504, UDP 480), make socket close a
one-way send. *Effect:* ~9× fewer round trips for small/medium packets; still one
crossing per packet, still two copies. Committed.

### Stage 2 — one shared frame per socket, MTU-sized, zero-copy payload (DONE — UDP + TCP)
Generalize the DNS `sendv`/`recvv` shared-frame path to every wire UDP/TCP socket: on
bind/connect, map a per-socket shared frame (a `NET_SHARED`-style region) into both the
app and the net server. `send`/`recv` write/read the payload *in the frame*; the IPC
carries only `(length, offset)` — **no payload copy across the boundary**, and full
MTU (up to ~1500 B) per packet. *Still one crossing per packet*, but the copy is gone
and the size limit is gone (fixes the residual >480 B truncation).

**UDP — built + validated (2026-06-20).** Done without any of the "missing
primitives": the key realization is the **socket cap is already per-socket badged**
(badge = socket id), and **handle transfer is a copy** (§3.4, sender retains). So the
net server allocates **one frame per socket id** lazily on first attach, maps it at
`frame_vaddr(sid) = NET_SHARED + sid*4096`, and **keeps it for its lifetime** — no
`frame_unmap` needed, because a reused socket slot re-shares the same physical page
with its new owner (16 sockets × 4 KiB = 64 KiB, fixed). The client (`rt::udp`) attaches
via the *socket* cap (not the control cap), so the server returns each socket its own
page — **correct per-process isolation**: two processes never map the same frame. The
attach reply returns `sid`, so the client picks a stable per-sid client vaddr
(`UDP_XFER + sid*4096`) and maps it exactly once (no remap). The cap-copy + per-sid
binding sidesteps `frame_unmap`/remap/sender-badge entirely.

- **Server** (`servers/net`): `socket_frames[sid]` table; `TAG_UDP_ATTACH` dispatched on
  the socket badge → alloc/map/return `(frame cap, sid)`; `TAG_UDP_SENDV`/`RECVV` read/
  write `frame_vaddr(sid)`; `RECVV` also returns the sender IPv4/port for `recv_from`.
- **rt** (`rt::udp`): `attach_sock(sock)` caches sock→ptr (so send/recv don't re-attach
  per packet) + a per-sid one-time mapping; `recvv_src` returns `(len, ip, port)`;
  `close` drops the cache entry. The hosted shims `__oxbow_udp_send_to`/`recv_from` use
  the frame for >480 B (send) and always for recv (no truncation); inline ≤480 stays.
- **Migrated to per-socket:** the DNS path (`dns_transport`), the shell `dns` builtin,
  and the c-ares glue (`cares_glue.c`, per-`g_fds[fd].frame`). The old single-frame
  control-cap `attach` is gone.
- **Validated:** `dns example.com/google.com` resolve on the wire (shell + rt + net);
  a std `UdpSocket` echo (`std-port/apps/udpmtu`) round-trips 400/800/1400-byte
  datagrams byte-for-byte off a host echo at 10.0.2.2 — the 800/1400 cases are the
  >480 frame path that previously truncated/rejected.

**TCP — built + validated (2026-06-20).** Same per-socket frame, generalized to the
byte stream: a connected TCP socket attaches its frame with `TAG_UDP_ATTACH` (the attach
is socket-type agnostic), then `TAG_TCP_SENDV`/`RECVV` move up to a full MTU per IPC
instead of the 504-B inline cap (~3× fewer domain crossings on bulk transfer). `SENDV`
returns smoltcp's accepted count (may be < requested when the tx buffer is full, so
`write_all` loops); `RECVV` consumes only `want` bytes (byte-exact — smoltcp keeps the
rest, which TLS needs).
- **Server** (`servers/net`): `TAG_UDP_ATTACH` now accepts `Sock::Tcp`; `TAG_TCP_SENDV`
  reads `frame_vaddr(sid)` → `tcp_stack.send` → (status, sent); `TAG_TCP_RECVV` →
  `tcp_stack.recv` into the frame.
- **rt** (`rt::tcp`): `sendv(sock,len)→Option<usize>`, `recvv(sock,want)→usize`, reusing
  `udp::attach_sock` + `udp::frame_drop` (the frame machinery is shared, type-agnostic);
  `close` drops the cache entry. The hosted shims `__oxbow_tcp_send`/`recv` use the frame
  for >504 B and stay inline for ≤504 (TLS's small header reads).
- **Validated:** a std `TcpStream` echo (`std-port/apps/udpmtu`) round-trips 200/1400/
  4000-byte payloads byte-for-byte off a host TCP echo at 10.0.2.2 — 1400 is one frame
  chunk, 4000 spans three (1472+1472+1056) via `write_all`/`read_exact`.

Stage 2 is now complete for both UDP and TCP. Next is Stage 3 (multi-slot ring +
batched doorbell) — the line-rate lever.

### Stage 3 — a multi-slot ring + batched doorbell (DONE — UDP)
Replace the single frame with a **ring of N fixed-size slots** + a TX and RX
descriptor ring (head/tail indices) in the shared region. The app fills K slots,
advances the TX tail, and rings the doorbell *once*; the net server drains all posted
TX slots and harvests buffered RX in that single handler. **One crossing per *batch*.**

**Built + validated (2026-06-20).** A second per-socket page (`ring_vaddr(sid)` =
`RING_BASE + sid*4096`, 1 MiB above the Stage 2 frames), attached with `TAG_UDP_RING`,
laid out as a TX ring + an RX ring (SPSC; the four u32 head/tail indices live in a
64-byte header, then `RING_SLOTS` TX slots and `RING_SLOTS` RX slots of
`RING_SLOT_STRIDE` bytes each — a single 4 KiB page, so slots are small: the
small-packet / high-pps case netmap actually targets). Layout + slot descriptor
(`ip`/`port`/`len`) are normative in `abi/src/lib.rs`.

- **Doorbell = a `TAG_UDP_KICK` sys_call** — which *is* netmap's model ("one syscall
  tells the NIC I queued N packets"), and fits oxbow's single-wait-source server loop
  without multiplexing a notification into it. The kick handler drains `tx_head..tx_tail`
  (one `send_udp` per slot) and harvests RX with the non-blocking `recv_udp_for` until
  the RX ring is full or nothing is buffered, then replies `(sent, harvested)`.
- **No data race:** the kick is a synchronous rendezvous, so the app is blocked while
  the server touches the ring — the two sides strictly alternate. Indices still use
  `AtomicU32` acquire/release for cross-process visibility (belt-and-suspenders over the
  syscall barrier); the head==tail-empty / (tail+1)==head-full discipline gives each
  ring a capacity of `RING_SLOTS-1`.
- **rt** (`rt::ring`): `attach(sock)→Ring`; `Ring::push(ip,port,payload)→bool` (pure
  memory write, NO IPC), `Ring::kick()→(sent,harvested)`, `Ring::pop(&mut buf)`. A
  native batch API — std's `UdpSocket` has no batch verb, so this is the layer where the
  per-batch win is expressible.
- **Validated:** `std-port/apps/udpmtu` queues 7 datagrams, kicks **once** (server sends
  all 7 in one crossing), and harvests all 7 echoes back through the RX ring byte-exact
  off the host echo at 10.0.2.2 — "7 sends, 1 TX crossing."

**Async RX wakeup — built + validated (2026-06-20).** So a ring app blocks instead of
poll-kicking for RX, the net server became **event-driven**, which needed a new kernel
primitive: **`sys_recv_notif(ep, notif, msg, timeout)`** — a multiplexed wait that blocks
until a message arrives on `ep` OR `notif` is signalled OR a timeout elapses. It's
race-safe by an asymmetry: a sender handoff deposits a return, but a notif/timer wake
only flips the thread Ready (no deposit) — mirroring the existing timer wake — so two
concurrent wakers of the same thread can't corrupt its return slot (if the sender wins,
the notif count just stays latched for the next call).
- **Kernel:** `notif` gains a `bound_waiter`; `signal` wakes it without depositing; the
  IPC `recv_notif` registers on the endpoint (covers senders, §70) then arms the notif,
  and on resume returns the message or `RECV_NOTIF_FIRED`.
- **Net server:** while a ring socket has a registered RX notif (`TAG_UDP_RXNOTIF`,
  `async_count > 0`) the main loop waits with `sys_recv_notif(BOOT_EP, nic.notif, …)`; on
  a NIC-IRQ/timeout wake it runs the pump (`pump_ring_rx`) to demux RX into the ring
  sockets and signals each one's notif. With `async_count == 0` the loop is byte-for-byte
  today's `sys_recv`, so non-async clients (curl/DNS/TCP) are unaffected. The KICK skips
  inline RX harvest for async sockets (RX is the pump's job — else it races it).
- **Two real-hardware gotchas fixed:** the e1000 RX IRQ is **shared (IRQ 11 with
  virtio-blk)** and doesn't fire reliably → the `timeout` arg gives a ~20 ms polling
  fallback layered under the IRQ. And fast localhost echoes were being grabbed by the
  KICK's own harvest before the pump saw them → KICK is TX-only for async sockets.
- **rt:** `sys_recv_notif` + `Ring::set_rxnotif(notif)`. **Validated:** the app registers
  a notif, sends a TX batch, blocks in `sys_notif_wait` (no poll-kick), and the pump
  delivers all 7 echoes + signals it — one wakeup, all 7 popped byte-exact.

**Pump feeds smoltcp — built + validated (2026-06-20).** The async pump owns the NIC, so
TCP frames it drained would have been lost (concurrent TCP degrades). Fixed with a bounded
**software RX queue inside the `Nic`** (`tcp_rx_q`): the pump routes IPv4-TCP frames into it
(ring-UDP still goes to the ring + notif; ARP/ICMP stay inline via `handle_background`), and
smoltcp's `PhyDevice::receive` drains that queue before the NIC ring. Crucially the queue is
**empty and untouched outside async mode** (only the pump fills it, and the pump only runs
when `async_count > 0`), so the non-async TCP path is byte-for-byte the unchanged
direct-from-NIC read — no regression. Validated: a 1000-byte TCP echo completes **while a
ring socket is in async mode**, with the echo deliberately delayed so it lands during a pump
tick — proving it flowed NIC → pump → `tcp_rx_q` → smoltcp.

**Non-ring UDP (the DNS case) too — built + validated (2026-06-20).** A sibling `udp_rx_q`
in the `Nic`: the pump parks non-ring UDP datagrams keyed by dst port (already-parsed
payload + source), and `recv_udp_for` **selectively** pulls only its own port (leaving other
sockets' datagrams queued). Same empty-outside-async / no-regression property as `tcp_rx_q`.
Validated by a 600-byte non-ring UDP echo completing while a ring socket is async (echo
delayed to land during a pump tick → NIC → pump → `udp_rx_q` → `recv_from`). *Surfaced a real
latent bug:* two live UDP sockets could share a port (oxbow std picks its own ephemeral and
`TAG_UDP_BIND` did no collision check, so the pump misdelivered). Fixed — bind now searches
upward for a free port and returns the actual one.

*Remaining for a fuller Stage 3:* MTU-sized slots need a multi-page ring (`MSG_HANDLES=4`
lets one attach carry up to 4 frame caps) — small slots were the deliberate first cut
(high-pps is the point). Slot typestate (`Slot<Owned>`/`Slot<Posted>`) is a nicety the
rendezvous alternation already makes safe.

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

Stages 1–3 are built and validated. Stage 2 (per-socket zero-copy MTU frame, UDP + TCP)
generalized the DNS path, fixed the last truncation limit, and removed the payload copy.
Stage 3 (the batched ring + doorbell, UDP) is the line-rate lever: one domain crossing
amortizes a whole batch of datagrams (proven: 7 sends, 1 crossing). Together they took
oxbow's networking from "correct and decent" toward "competitive" — by copying netmap's
**data-plane design**, not its (or any) C network stack. What's left is depth, not
direction: MTU-sized multi-page rings, an async notif doorbell, and Stage 4 (NIC DMA
straight into the ring slots — true zero-copy wire-to-app).
