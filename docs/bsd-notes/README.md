# What oxbow can borrow from the BSDs — synthesis

Reference notes from examining FreeBSD (performance), OpenBSD (security), and NetBSD
(portability), each mapped concretely to oxbow — a Rust capability microkernel. Read
the per-project files for detail; this page is the cross-cutting synthesis and the
single prioritized to-do list.

- [`freebsd-performance.md`](freebsd-performance.md)
- [`openbsd-security.md`](openbsd-security.md)
- [`netbsd-portability.md`](netbsd-portability.md)
- [`../posix-compliance.md`](../posix-compliance.md) — the POSIX-compatibility analysis

## The one cross-cutting theme

**oxbow being a Rust capability microkernel changes which BSD lessons transfer.** A
huge fraction of BSD engineering exists to compensate for being a *C monolith with
ambient authority*:

- OpenBSD's memory-corruption mitigations (RETGUARD, SSP, malloc canaries, MAP_STACK)
  re-impose at runtime the safety **Rust already gives at compile time** → mostly
  **redundant** for oxbow's native code.
- FreeBSD's lock zoo (rmlock/sx/rw/mutex hierarchy) is a symptom of **shared mutable
  kernel state** → oxbow's per-server isolation means you want *fewer* primitives,
  not six.
- NetBSD's decades of MI/MD discipline produce portability oxbow **gets ~94% for free**
  from Rust + userspace servers; its rump/anykernel goal (run fs/net as userspace
  libraries) **oxbow already has structurally**.

So the rule is: **take the orthogonal insights, skip the C-monolith compensations.**

## The unified priority list (do these)

### Security (orthogonal to Rust — still needed)
1. **A real kernel CSPRNG** (ChaCha20-class, seeded from RDRAND/RDSEED/virtio-rng/
   jitter + persisted seed, reseeded). **HIGH.** It underwrites *everything*: the
   ASLR slide, stack gaps, and the unforgeability of capability badges. Make it the
   single source `__oxbow_getentropy` bottoms out in. Weak RNG silently defeats all
   other properties at once.
2. **Immutable/sealed mappings** (mimmutable-style): seal `.text`=X / `.rodata`=R
   one-way after load so W^X is un-revokable; gate the tcc/JIT W→X exception behind an
   unforgeable "may map executable" capability. **HIGH** — language-agnostic; Rust
   doesn't stop a logic/`unsafe` bug from flipping page perms.
3. **Treat the from-scratch byte-parsers as the crown-jewel attack surface** (ARP/IP/
   TCP/DHCP, DNS/c-ares, ext2 metadata): run them as the most-isolated, fewest-cap
   servers. The privsep *discipline*, not the fork/setuid mechanism. **HIGH concept.**
4. **Harden the C world**: tcc emits no stack cookies/RETGUARD, so on-device-compiled
   binaries are the softest spot → OpenBSD-malloc tactics *inside oxbow-libc* + enable
   CET/IBT system-wide as language-agnostic CFI. **MED.**

### Performance (the microkernel IPC tax is the whole game)
5. **netmap-style shared-ring data plane with a batched doorbell**, repurposed from
   kernel↔user to **server↔server** (app↔net↔driver): establish a shared frame once,
   carry packets as ring-slot indices, use IPC only as a batch wakeup — never per
   packet. **HIGH** — this is *the* lever between line-rate and toy networking, and it
   drives the zero-copy frame plumbing already noted as missing (`frame_unmap`, remap,
   sender-id on recv).
6. **Per-CPU cached allocation in two layers**: adopt `snmalloc`/`mimalloc`
   (message-passing-aware) as the userspace `GlobalAlloc`, and a small UMA-style
   per-CPU slab for fixed-size kernel objects (cap slots, IPC buffers, TCBs). **HIGH**
   — lock-free, atomic-free, architecture-neutral, pure win.
7. **Epoch-based reclamation** (`crossbeam-epoch`) *server-local* for read-mostly
   tables (ARP/route/socket lookup). **MED** — the right lockless-read primitive,
   *more* natural in Rust than C; don't build a giant generic kernel epoch.
8. **Observability early**: x86 PMC counters (a few MSRs behind one cap) + static
   tracepoints (IPC send/recv, syscall enter/exit, sched switch, ring doorbell) into a
   per-CPU ring drained by a userspace tracer. **MED** — your costs are invisible
   domain crossings; this is how you know items 5–6 paid off (it already caught the
   "1800 syscalls per 100 KB read").

### Portability (cheap insurance, do before a 2nd arch)
9. **Extract a `Pmap`/`Arch` trait and seal the 6 MD leaks** (`mm/vm.rs`,
   `usermem.rs`, `percpu.rs`, `smp.rs`, `pci.rs`, `rng.rs`); replace hardcoded `4096`
   with `arch::PAGE_SIZE`. **HIGH** — this *is* the aarch64 porting cost; done right,
   a 2nd arch becomes "fill in `arch/aarch64/`."
10. **Wrap MMIO/DMA in `bus_space`/`bus_dma`-style types now** (with only 2 drivers):
    an `Mmio` type with `barrier()`, a `DmaRegion` exposing device-addr separately +
    `sync_for_device/cpu` (no-ops on x86, real barriers/cache-ops on aarch64). **HIGH**
    — cheap now; without it the first aarch64 boot has silent DMA/MMIO-ordering
    corruption that's brutal to debug.
11. **Codify the anykernel rule**: a server may depend only on the syscall ABI + its
    granted caps — never on the arch, never on a self-computed physical address.
    **HIGH concept / LOW work** — it's what keeps all 44 servers + libc recompile-only
    across arches.

### Skip / later
- Skip: RETGUARD/SSP/malloc-canaries for native Rust, FreeBSD's lock zoo + TCP fast
  path, full DTrace, KARL per-boot relinking, NetBSD `build.sh`/pkgsrc (Rust target
  triples already cross-build; revisit pkgsrc as an app-distribution model once
  there's real package volume).

## How this dovetails with POSIX

The POSIX analysis (`../posix-compliance.md`) and these notes point the same way:
oxbow's job is **not** to imitate a Unix kernel but to be a capability core with
*personalities* layered on top. The BSD lessons that fit are the ones that respect
that — netmap rings, per-CPU allocators, sealed mappings, a real CSPRNG, the MI/MD
trait split — and the security punchline is the same as POSIX's: software ported onto
oxbow ends up **more confined than on a BSD**, because every process is born
`pledge`'d/`unveil`'d by which capabilities it holds.
