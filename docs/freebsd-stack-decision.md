# Should oxbow port FreeBSD's TCP/IP + UDP/IP stack? — No. Here's why, and what to do instead.

*Decision note. 2026-06-20. You asked about pulling FreeBSD's networking stack
(specifically the TCP/IP + UDP/IP stacks) into oxbow. Short answer: don't port the C
code — it's the wrong unit of reuse for a Rust microkernel. Take the **design**, keep
the **Rust stack**.*

## Why porting the FreeBSD C stack is the wrong move

1. **It is not "a stack" — it's a slice of the FreeBSD kernel.** FreeBSD's
   `sys/netinet` (TCP/IP) + `sys/netinet/udp_*` are ~150–200 KLOC of C that assume the
   *entire* FreeBSD kernel underneath them:
   - **mbuf** — the kernel's chained packet-buffer memory model, woven through every
     function. Porting "the stack" means porting mbufs, which means porting the
     kernel's memory allocator semantics.
   - **The socket layer + PCB tables** (`sys/kern/uipc_socket.c`, `in_pcb`) — the
     stack doesn't terminate at a clean API; it terminates in the socket buffer and
     protocol-control-block machinery, which assume the VFS, the file-descriptor
     table, and `sysctl`.
   - **The locking model** — `INP_WLOCK`, `NET_EPOCH`, the per-PCB locks, the routing
     table locks. This is shared-mutable-state, monolith-kernel locking. A microkernel
     has none of it and wants none of it.
   - **Routing, `rtentry`, ARP, IPsec hooks, `pfil`, netisr** — the stack reaches
     sideways into a dozen other kernel subsystems.
   You cannot extract the TCP state machine without dragging the kernel it lives in.

2. **The impedance mismatch with a Rust capability microkernel is total.** You'd be
   FFI-wrapping C that wants kernel addresses, kernel locks, kernel allocators, and a
   shared address space — inside a memory-safe, isolated-server, capability OS. The
   `unsafe` surface alone would dwarf oxbow's entire current `unsafe` footprint and
   re-introduce exactly the C memory-unsafety oxbow was built to avoid. (See
   `docs/bsd-notes/openbsd-security.md`: the whole point of Rust here is to *not* run
   attacker-controlled bytes through C parsers.)

3. **oxbow already has a good stack.** It runs **smoltcp** (a mature, widely-used,
   memory-safe, `no_std` Rust TCP/IP stack — SACK, windows, timers, IPv4/IPv6) for TCP,
   plus its own hand-written, memory-safe IPv4/UDP/ICMP/ARP/DHCP. This already does
   real wire TCP/UDP/DNS to the internet (proven by httpd + the test suites). It is the
   *right shape* for a microkernel: small, isolated, safe, embeddable in the net
   server.

4. **FreeBSD's network *performance* is not in the protocol code — it's in the data
   plane.** What makes FreeBSD fast is mbuf zero-copy, **netmap**, RSS, `SO_REUSEPORT`
   load-balancing, and epoch-based lockless lookups — the *plumbing around* the stack,
   not the TCP state machine. That plumbing is a **design**, and it transfers to
   oxbow's stack directly (see `docs/netmap-data-plane.md`). Porting the C TCP code
   would get you FreeBSD's *protocol behavior* (which smoltcp already has) and **none**
   of FreeBSD's *speed* (which lives in the data plane you'd still have to build).

## What to actually take from FreeBSD's networking

Take the **design ideas**, apply them to smoltcp + the oxbow net server — all detailed
in `docs/bsd-notes/freebsd-performance.md`:

- **netmap-style shared-ring data plane** (the big one) — `docs/netmap-data-plane.md`.
  This is the real "FreeBSD networking performance" you want, and it's stack-agnostic.
- **Epoch-based reclamation** (`crossbeam-epoch`) for the read-mostly lookup tables
  (ARP cache, routes, the socket/PCB table) — lockless reads, the FreeBSD `NET_EPOCH`
  idea, *more* natural in Rust.
- **RSS / multi-queue + `SO_REUSEPORT`-style load balancing** across cores, once SMP
  packet processing matters.
- **mbuf's insight, not its code**: a chained/segmented zero-copy buffer type so a
  packet's headers and payload needn't be contiguous or copied — Rust can express this
  as a safe `&[IoSlice]`-like scatter-gather over shared-frame slots.

## When (if ever) to revisit a different stack

Stay on smoltcp. If oxbow ever needs something smoltcp lacks (e.g. a specific
congestion-control algorithm, TCP Fast Open, advanced offload), the order of
preference is:
1. **Extend smoltcp** (it's Rust, hackable, and you already vendor/patch it).
2. **Port/adapt another clean Rust stack** if one fits better.
3. **A heavily-adapted, sandboxed subset of a C stack** — only as a last resort, and
   only run inside its own most-isolated, fewest-capability server (treat it like the
   hostile byte-parser it is).

Porting FreeBSD's monolithic C stack wholesale into the kernel is never on this list.

## Bottom line

**No to the C stack.** Keep smoltcp + the hand-written IPv4/UDP. Pull FreeBSD's
*data-plane design* (netmap ring, EBR, RSS) onto it — that's where the performance
actually is, and it's the version that keeps oxbow safe and isolated instead of
bolting a slice of a C monolith into a Rust capability kernel.
