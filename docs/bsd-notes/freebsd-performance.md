# FreeBSD Performance Engineering — Reference Notes for oxbow

Actionable notes mapping FreeBSD's performance-critical subsystems onto **oxbow**:
a from-scratch capability microkernel in Rust (x86_64, seL4-leaned synchronous-rendezvous
IPC, zero ambient authority, capabilities-as-handles). oxbow already has its own e1000
driver, hand-written ARP/IPv4/ICMP/UDP + smoltcp TCP, an ext2 fs server (lwext4) over
virtio-blk, a futex-based SMP scheduler, a userspace window server, and a near-complete
Rust std port. **Drivers and the filesystem are userspace servers reached via IPC.**

The recurring theme below: FreeBSD is a *monolithic* kernel. Most of its fastest tricks
work because the network stack, allocator, and drivers share one address space and one
trust domain. oxbow's wins and losses both come from *not* having that. The highest-value
ideas are the ones that are **architecture-neutral** (allocator caching, batching, epoch
reclamation) or that **lean into** the microkernel boundary (shared-memory rings to amortize
IPC, the exact problem netmap already solved for the kernel/user boundary).

---

## Top takeaways for oxbow (executive summary)

1. **Your IPC boundary is netmap's problem.** A microkernel pays a domain crossing where a
   monolith pays a function call. The single most impactful idea here is to make the
   data-plane path between app ↔ net-server ↔ driver a **shared-memory ring of preallocated
   buffers with batched, amortized notifications** — exactly netmap's design, repurposed
   from "kernel↔user" to "server↔server." This is the difference between oxbow being a toy
   and oxbow doing line-rate.

2. **Per-CPU cached allocation (UMA's magazines / jemalloc's tcache) transfers cleanly and
   is high-ROI.** It's architecture-neutral: a fast path that satisfies most alloc/free from
   a CPU-local cache with no atomics and no lock. This belongs in *two* places in oxbow — the
   userspace malloc (your std/rt) and any in-kernel object pools (capability slots, IPC
   message buffers, page-frame metadata).

3. **Epoch-Based Reclamation (EBR) is the right concurrency primitive for your lockless read
   paths**, and it's *more* natural in Rust than in C (crossbeam-epoch already exists, battle-
   tested). Use it inside the network server and any multi-reader table (routing/ARP/socket
   lookup) so readers never take a lock or write a cache line. Don't put a giant generic EBR
   in the kernel; keep it server-local.

4. **Most of FreeBSD's lock zoo (rmlock, sx, the mutex hierarchy) is a symptom you should aim
   to avoid, not a feature to copy.** They exist to make a giant shared-address-space kernel
   safe. oxbow's design goal — small kernel, single-threaded-ish critical sections, state
   partitioned per-server — means you want *fewer* lock types: per-CPU data + one good
   sleepable lock + EBR for read-mostly. Copy the *insight* (read-mostly data wants reader-
   free fast paths), not the six implementations.

5. **Observability is a force multiplier you can build cheaply.** You can't run DTrace, but
   you can expose hwpmc-style PMC counters and a static-tracepoint/ring-buffer mechanism
   over your existing IPC. Measure IPC round-trip cost, scheduler latency, and allocator
   hit-rate *before* optimizing — FreeBSD's whole perf culture is "measure with DTrace/pmc
   first." Without numbers you'll optimize the wrong crossing.

---

## 1. Memory allocation

### 1.1 jemalloc (userspace malloc)

**What it is.** FreeBSD's libc malloc. Core ideas: memory is split into per-thread/per-CPU
**arenas** to cut contention; allocations are rounded to a fixed set of **size classes**
(8, 16, 32, 48, 64, … geometric-ish spacing) so each class has its own free list and
fragmentation is bounded; each thread has a **tcache** (thread cache) of recently-freed
objects per size class, refilled/flushed in batches from the arena. Small allocations come
from "slabs" (runs of pages carved into same-size slots); large ones go straight to mapped
extents. Metadata is kept out-of-band so the data path stays cache-dense.

**Why it's fast / key insight.** The common case — alloc/free of a small object on a hot
thread — touches *only* the thread cache: no lock, no atomic, no cross-core cache-line
bounce. Contention is pushed to the rare slow path (tcache miss → arena, which is sharded).
Size classes turn "find a fitting hole" (O(n), fragmenting) into "pop a free-list head"
(O(1), bounded waste).

**Applicability to oxbow.** Directly relevant — this is *userspace*, exactly where your Rust
std port lives. Rust programs already funnel through `GlobalAlloc`. You have three options:
(a) keep a simple bump/freelist allocator (fine for now), (b) port jemalloc's design ideas
into a small Rust allocator, or (c) adopt an existing Rust allocator with the same shape
(`mimalloc` and `snmalloc` are arguably better-engineered modern equivalents; snmalloc is
*message-passing aware*, designed so freeing across threads is cheap — interesting for a
message-passing OS). The arena/tcache split maps cleanly because it needs nothing from the
kernel except "give me more pages" — which oxbow already provides as a capability-mediated
map operation. The one wrinkle: jemalloc assumes cheap `mmap`/`madvise`. In oxbow, growing
an arena is a syscall that mediates a frame capability; make sure your "get more memory"
path is cheap and batched (grab many pages per slow-path trip).

**Priority: HIGH** — every userspace program allocates; a per-thread-cache allocator is the
single biggest userspace perf lever and is pure-userspace work (no kernel changes). Prefer
adopting `mimalloc`/`snmalloc` over hand-rolling jemalloc.

### 1.2 UMA — the kernel zone/slab allocator

**What it is.** "Universal Memory Allocator," FreeBSD's in-kernel allocator. A **zone**
allocates one type of object (fixed size, optional ctor/dtor, alignment). A **keg** (refinement
introduced for network buffers) describes the *backing* page format; a zone can front multiple
kegs (e.g. one keg from normal pages, one from superpages for fewer TLB entries). Each zone
keeps **per-CPU caches** ("buckets"/magazines) of free objects; alloc/free hit the per-CPU
bucket first, falling back to a zone-global cache, then to the slab/keg layer, then to the VM.
The ctor/dtor + "keep objects type-stable" design lets objects retain partial initialization
across free/realloc.

**Why it's fast / key insight.** Same core trick as tcache but in the kernel: per-CPU buckets
make the hot path lock-free and atomic-free; type-specific zones avoid generic-malloc overhead
and let you cache *constructed* objects (skip re-init). Multi-keg lets hot object types live in
superpages → fewer TLB misses on the network fast path.

**Applicability to oxbow.** Two distinct homes:
- **In-kernel object pools.** Your microkernel still allocates: capability-table slots, IPC
  endpoint/message structures, thread/TCB structs, page-frame metadata. A small UMA-style
  per-CPU-cached slab allocator for these *fixed-size* kernel objects is a strong fit and very
  much in the spirit of a minimal kernel (bounded, no fragmentation, fast). You already have
  SMP, so per-CPU buckets pay off.
- **In servers.** The fs server (lwext4) and net server churn buffers; a zone-per-object-type
  cache inside each server is the userspace analog.

The *multi-keg/superpage* refinement is lower priority until you have a working superpage/2MB
mapping path — but worth noting as the eventual win for mbuf-equivalent network buffers.

**Priority: HIGH** for a per-CPU slab allocator for kernel objects (cap slots, IPC buffers,
TCBs) — it's small, bounded, and on every hot kernel path. Superpage-backed kegs: LOW until
2MB page support exists.

---

## 2. Scheduler — ULE

**What it is.** FreeBSD's default scheduler (`SCHED_ULE`). Key properties:
- **Per-CPU run queues**, not a global one. Each CPU schedules from its own queue → no global
  scheduler lock on the common path, scales with cores.
- **Interactivity scoring.** Tracks each thread's recent sleep-vs-run ratio; "interactive"
  threads (mostly sleeping, e.g. UI, shells) get a temporary priority boost so they preempt
  CPU-bound work and feel responsive. Scoring is a cheap voltage-style heuristic recomputed on
  sleep/wake.
- **CPU affinity.** A thread prefers the CPU it last ran on (warm cache/TLB). Affinity is
  weighted by how recently it ran and cache topology (SMT siblings, shared LLC).
- **Work stealing / load balancing.** Idle or imbalanced CPUs pull runnable threads from busier
  CPUs' queues, periodically and on idle, respecting affinity cost.
- Separate run queues for timeshare/interactive vs. realtime/idle bands.

**Why it's fast / key insight.** Per-CPU queues remove the central contention point; affinity
preserves cache/TLB warmth (the dominant real-world cost of migration); work stealing keeps
cores busy without a global lock. Interactivity scoring buys responsiveness almost for free.

**Applicability to oxbow.** oxbow already has a futex-based SMP scheduler, so the relevant
question is *which ULE ideas to graft on*:
- **Per-CPU runqueues + work stealing: HIGH-value, directly applicable.** If oxbow currently
  has a global runqueue or global scheduler lock, this is the #1 scheduler scalability fix.
  Lives in the kernel. Moderate effort: per-CPU `struct cpu` runqueue, an idle-CPU steal path,
  a periodic balancer.
- **Affinity (last-CPU preference): HIGH, cheap.** Just remember `last_cpu` per thread and
  prefer it at wake. Big real win because cache warmth dominates. Pure kernel bookkeeping.
- **Interactivity scoring: MED.** Valuable once oxbow runs an interactive desktop (it has a
  window server). The sleep/run-ratio heuristic is small and self-contained. But be skeptical:
  a capability microkernel often wants *predictable*/explicit scheduling (donation, priorities
  set by the parent) over a guessing heuristic. Consider whether **priority/time-slice donation
  across IPC** (seL4-style) is a better fit than ULE's autodetection — when a client IPCs a
  server, the server should run on the client's priority/budget. That's more in oxbow's spirit
  than interactivity guessing.
- **Skeptical note:** ULE assumes the kernel owns all scheduling policy. In a capability system
  you may eventually want scheduler *activations* or user-level scheduling contexts handed out
  as capabilities (seL4-MCS sched contexts). Don't over-invest in in-kernel heuristics if the
  long-term design is to export scheduling as a resource.

**Priority: HIGH** for per-CPU runqueues + affinity + work stealing (scalability + cache
warmth, pure kernel). MED for interactivity scoring; instead prioritize **IPC priority/budget
donation**, which fits the microkernel better.

---

## 3. Network stack performance

This is the richest area and the one where the microkernel boundary matters most.

### 3.1 mbuf design

**What it is.** The `mbuf` is FreeBSD's fundamental network buffer: a small (256B) fixed
struct holding a bit of inline data + metadata. Bigger payloads attach an external **cluster**
(2KB/4KB/9KB/16KB) via `m_ext`, refcounted so clusters can be shared without copying. Packets
are **chains** of mbufs (`m_next`); a packet header mbuf (`m_pkthdr`) carries length, receive
interface, checksum-offload flags, RSS hash, etc. `m_pullup` linearizes the bytes a protocol
needs to read; prepend/adj add/remove headers by moving pointers, not copying. mbufs come from
dedicated UMA zones (`m_getcl` pulls an mbuf+cluster pair from per-CPU caches).

**Why it's fast / key insight.** (1) Header push/pop is pointer math, not memmove — you prepend
Ethernet/IP/TCP headers by walking *backwards* into reserved leading space. (2) Refcounted
external clusters give zero-copy sharing (e.g. one buffer fanned out to multiple sockets, or
retransmit queues). (3) Fixed sizes + per-CPU UMA caches make alloc/free fast and
non-fragmenting. (4) Scatter-gather chains avoid one big contiguous allocation per packet.

**Applicability to oxbow.** You wrote your own ARP/IP/ICMP/UDP, so you have *some* buffer
representation today. The mbuf ideas worth stealing:
- **Reserve leading headroom** in your buffer so the stack pushes headers down-stack without
  copying. Cheap, high-value, do it now.
- **Refcounted shared payloads** for zero-copy fanout/retransmit. Rust makes this clean (`Arc`-
  like, or an explicit refcount) and *safer* than C's hand-rolled `m_ext`.
- **Scatter-gather chains** — only worth it if you support large/jumbo or TSO/GRO; for an MTU-
  sized stack a single buffer is simpler. Be skeptical of importing mbuf-chain complexity before
  you need it.
- **The big microkernel wrinkle:** in oxbow the buffer lives in a *server's* address space, and
  the driver lives in *another* server (or kernel). The mbuf design assumes one shared address
  space where a pointer chain is universally valid. Your buffers must be **offsets into a
  shared-memory region**, not raw pointers — which leads straight to netmap (3.2).

**Priority: MED-HIGH** — adopt headroom + refcounted payloads now (cheap, clean in Rust). Defer
mbuf-chain scatter-gather until TSO/jumbo. Represent buffers as region offsets, not pointers.

### 3.2 netmap — zero-copy packet I/O (the most important idea here)

**What it is.** A framework (Luigi Rizzo, USENIX ATC '12) that hits **14.88 Mpps on one core**
on a 10G NIC by attacking the three real packet costs: per-packet allocation, per-packet
syscall overhead, and copies. Mechanism: a **shared memory region** (`mmap`'d once) holds
preallocated packet **buffers** and a set of **rings** (circular arrays of slots; one ring per
NIC hardware queue). Each slot points (by index) to a buffer in the shared region. Userspace
and kernel both see the same rings/buffers. To send/receive you fill/drain slots and advance
ring head/tail pointers; a single `ioctl`/`poll` (the "sync") hands a **whole batch** of slots
across the boundary at once. Device registers stay protected (kernel validates ring indices);
only the data buffers are shared. Includes VALE (in-kernel software switch) and netmap pipes
(shared-memory channel between processes).

**Why it's fast / key insight.** *Amortize the boundary crossing over a batch, and never copy.*
Buffers are preallocated (no per-packet malloc); one syscall moves N packets (syscall cost / N
→ ~0); data is shared not copied. The ring is a lock-free SPSC structure (producer owns head,
consumer owns tail) so the two sides synchronize with two cache lines, no locks.

**Applicability to oxbow — THIS IS THE CENTERPIECE.** netmap was invented to cheapen the
*kernel↔userspace* boundary. oxbow has the *same boundary, twice*: app↔net-server and
net-server↔driver, plus you pay an IPC instead of a syscall, which is *more* expensive. So
netmap's design is not just applicable — it is arguably *necessary* for oxbow to ever do
serious throughput. Concretely:
- Set up a **shared-memory ring region** between each pair of communicating servers (app↔net,
  net↔driver), established *once* at connection setup by passing a **frame/region capability**
  over IPC. After that, the data plane is pointer-free ring manipulation in shared memory.
- The synchronous-rendezvous IPC becomes the **"sync" doorbell**, sent only when a batch is
  ready or the ring would otherwise idle — *not* per packet. This is how you stop paying an IPC
  per packet. (Your seL4-leaned IPC is fine as the notification; the bulk data never rides the
  IPC.)
- Buffers are **preallocated in the shared region**; slots carry indices, not capabilities, so
  there's no per-packet capability churn.
- The DMA-capable driver server already owns the NIC's rings; netmap-style you expose *its*
  ring (or a shadow) to the net server. The e1000 already has descriptor rings — this is a
  natural fit.

**Skeptical notes / what doesn't transfer for free:** (1) Shared memory between mutually-
distrusting servers needs validation — the consumer must treat ring indices/lengths as
untrusted and bounds-check every slot (netmap's kernel does exactly this; you do it at the
server boundary). Zero-copy across a trust boundary means you cannot also assume the buffer is
immutable unless you revoke write access — decide per link whether the producer keeps write
access (TOCTOU risk) or transfers it. (2) seL4-style IPC is *synchronous rendezvous*; netmap's
async batching wants the producer to keep filling while the consumer drains. You'll likely want
the ring to be the async buffer and IPC only the wakeup — i.e. don't rendezvous per packet.
(3) True zero-copy all the way to the *application* means the app maps the same buffers — great
for a packet-processing app, but a normal `recv()` into an app's own buffer still copies once
at the very edge. That last copy is usually fine.

**Priority: HIGH (highest in this document).** This is the idea that determines whether oxbow's
networking is microkernel-tax-bound or competitive. Build a shared-ring + batched-doorbell data
plane between app/net/driver. Largest single lever; also the most work.

### 3.3 Epoch-Based Reclamation (epoch(9) / ck_epoch)

**What it is.** FreeBSD's mechanism for safe lockless reads of mutable shared structures
(routing table, interface list, firewall rules, etc.). Readers wrap accesses in
`epoch_enter`/`epoch_exit` (which "never block" and take no locks — just bump a per-CPU epoch
counter). Writers update with atomics/copy-then-swap, then call `epoch_call`/`epoch_wait` to
defer freeing the old version until a **grace period** has elapsed — i.e. until every CPU that
*might* have held a reference has passed through a quiescent state. It's a deferred-reclamation
scheme (RCU-family). Backed by Concurrency Kit's `ck_epoch`.

**Why it's fast / key insight.** Readers pay *almost nothing* — no lock, no atomic CAS, no
cache-line write contention (just a per-CPU counter). Reclamation is deferred so writers never
have to coordinate with readers synchronously. Perfect for **read-mostly** data: many lookups,
rare updates.

**Applicability to oxbow.** Strong fit, and *better in Rust than in C*: `crossbeam-epoch` is a
mature, safe EBR library used widely in the Rust ecosystem (and the Aaron Turon "lock-freedom
without GC" work was literally Rust). Where to use it in oxbow:
- Inside the **network server**: ARP cache, routing/neighbor table, socket lookup hash, smoltcp
  interface set — all read-mostly. EBR lets the RX hot path do lookups without locking against
  the rare control-plane update.
- Inside the **kernel** *sparingly*: a read-mostly capability/endpoint table could use a per-CPU
  EBR, but keep the kernel's use minimal and audited — a microkernel wants its kernel simple.
  Prefer pushing EBR into servers.
- **Don't** build one giant global epoch like FreeBSD's net-epoch; keep epochs *scoped per
  data structure / per server* so grace periods are short and reclamation is local.

**Skeptical note:** EBR's grace period needs all participating threads to periodically reach a
quiescent state; a thread stuck in a long epoch section stalls reclamation (memory grows). In a
server with a clear request loop this is natural (quiescent between requests). Make sure you
don't hold an epoch across an IPC/block.

**Priority: HIGH (for servers).** Use `crossbeam-epoch` in the net server for read-mostly
tables. It's mostly free reads on the hottest path and Rust already has the library. MED/LOW for
in-kernel use — keep it scoped and minimal.

### 3.4 RSS (Receive Side Scaling)

**What it is.** NIC hardware hashes each packet's flow tuple (src/dst IP+port) and steers it to
one of N hardware RX queues, each tied (via MSI-X) to a different CPU. FreeBSD's RSS framework
aligns its software flow-to-CPU mapping with the NIC's hash so a connection is processed end-to-
end on one CPU (no cross-CPU handoff, warm cache, per-CPU PCB structures).

**Why it's fast / key insight.** Parallelism without locks: different flows go to different
cores by hardware, and each flow stays on one core so its socket/PCB state isn't shared or
locked across cores. The hash + queue steering is free (done in the NIC).

**Applicability to oxbow.** Conceptually applicable but gated by hardware. The e1000 (82540-era)
oxbow currently drives has *very* limited or no RSS/multi-queue; real RSS needs a modern multi-
queue NIC (igb/ixgbe/virtio-net with mq). So:
- **Today: LOW** — the hardware isn't there, and a single-queue 1G NIC won't saturate a core.
- **Design-forward: MED** — keep the *principle* in mind: when you add a multi-queue driver,
  map each HW queue to a CPU and run a per-CPU slice of the net server so flows stay core-local
  (mirrors RSS + per-CPU PCBs). virtio-net multi-queue under your Proxmox deploy is the realistic
  first target. Until then, don't build RSS machinery.

**Priority: LOW now / MED when a multi-queue NIC exists.** Gated entirely on driver/hardware.

### 3.5 SO_REUSEPORT / SO_REUSEPORT_LB

**What it is.** Multiple sockets bind the *same* port; the kernel load-balances incoming
connections/datagrams across them (classic `SO_REUSEPORT` allows the bind; FreeBSD's
`SO_REUSEPORT_LB` adds a hash-based load-balancing group). Lets you run N worker threads/
processes each with its own listening socket and accept queue → no single-accept-lock
bottleneck, scales accept across cores.

**Why it's fast / key insight.** Removes the "thundering herd / single accept queue" contention
point: each worker has a private queue, and the kernel spreads load by flow hash, pairing
naturally with RSS so the worker on the RSS CPU gets the flow.

**Applicability to oxbow.** This is a *socket API* feature, so it lives in your net server +
socket capability API. It's genuinely useful for a multi-process server design and fits the
microkernel: each server process holds its own listening socket capability, and the net server
spreads accepts across the group by flow hash. Modest effort, no kernel change (it's net-server
policy). Most valuable *after* you have per-CPU net-server slices / multi-queue.

**Priority: MED.** Clean fit in the socket-cap API, enables multi-worker servers; not urgent
until you're scaling a real network service across cores.

### 3.6 TCP/IP fast paths

**What it is.** Hand-optimized common-case paths: header prediction (`tcp_input` fast path for
in-order, expected-ACK segments skips the full state machine), precomputed/incremental checksums
with NIC checksum offload, LRO/TSO (large receive/segment offload — coalesce or split segments
in the NIC to amortize per-packet stack cost), and per-connection state laid out to minimize
cache misses.

**Why it's fast / key insight.** The overwhelmingly common packet (in-order data/ACK on an
established connection) takes a short straight-line path; offload pushes per-byte work
(checksum) and per-segment work (segmentation) into hardware.

**Applicability to oxbow.** You use **smoltcp** for TCP, so smoltcp's design governs this, not
you. smoltcp is already reasonably tight but is not a high-offload stack. Realistic levers:
- **Checksum offload** if/when a driver supports it (virtio-net does) — let the NIC do RX/TX
  checksums, set the smoltcp "checksum not needed" flags. MED, gated on driver.
- **Header room / avoiding copies** (covered in 3.1) matters more for oxbow than micro-
  optimizing the TCP state machine.
- Be skeptical of porting FreeBSD's `tcp_input` fast path — it's deeply tied to mbuf/PCB layout
  and you're not running that stack.

**Priority: LOW-MED.** Mostly smoltcp's domain; pursue checksum/TSO offload only when a capable
driver lands. Don't reimplement FreeBSD TCP.

---

## 4. VFS / buffer cache / vnode lifecycle

**What it is.** FreeBSD has a **unified buffer cache**: file data pages live in the VM page
cache (`vm_object` pages), and the buffer cache (`struct buf`) is a layer for block I/O that
shares those same pages — so `mmap` and `read()` see one coherent copy, no double-caching.
**Vnodes** (`struct vnode`) are the in-kernel handle for a file/dir; they're reference-counted
(`vref`/`vrele`), cached on a free list and recycled (`vnlru` reclaims idle vnodes under
pressure), and protected by per-vnode locks with a defined lock order. The **name cache**
(`namei`/`vfs_cache`) caches path→vnode lookups to avoid re-walking directories.

**Why it's fast / key insight.** (1) Unified cache = no copy/coherence problem between read()
and mmap, and one pool of memory for both. (2) Name cache turns repeated path lookups into hash
hits. (3) Vnode recycling avoids constant alloc/free of file metadata. (4) Page-cache readahead
+ clustering turn random-looking access into sequential I/O.

**Applicability to oxbow.** Your fs is a **userspace server (lwext4 over virtio-blk)**, so this
maps to *server-internal* caching, not a kernel buffer cache:
- **A block/page cache inside the fs server** is high-value: your notes already mention a
  one-file block cache fixing slow `/bin` loads. Generalize that to a proper LRU page cache
  keyed by (inode, offset) — that's the buffer cache idea, scoped to the server.
- **A name cache** (path → inode) inside the fs server avoids re-walking ext2 directory blocks
  per `open`. Cheap, high-value given your slow path-based reads.
- **Unified buffer cache (sharing pages between mmap and read):** only relevant once oxbow has
  file-backed `mmap`. If/when you do, the *insight* (one set of physical frames serves both the
  mapping and read/write, mediated by frame capabilities) is elegant in a capability system —
  the fs server hands the same frame capability to the mapper and uses it as its cache page.
  That's a genuinely nice capability-native version of the unified cache. Until mmap exists:
  defer.
- **Vnode lifecycle:** your equivalent is "open-file handle / inode cache" in the fs server.
  Reference-count and recycle inode structs; cache hot inodes. Standard server hygiene.

**Skeptical note:** FreeBSD's vnode locking complexity (lock order, `vget`/`vhold` races) is a
*monolithic-kernel shared-state* problem. In oxbow the fs server is (likely) single-threaded or
coarse-locked per request, so you sidestep most of that — don't import the vnode lock zoo.

**Priority: HIGH** for an in-server LRU page cache + name cache (you already felt this pain with
`/bin` loads). MED for inode caching/recycling. Unified-cache-via-shared-frames: MED and
contingent on file-backed mmap.

---

## 5. Locking primitives & lockless data structures

**What they are (the FreeBSD lock zoo).**
- **mutex (`mtx`)** — the workhorse. *Spin* mutexes (for code that can't sleep, e.g. interrupt/
  scheduler context) and *sleep* (default, adaptive: spins briefly if the holder is running,
  else sleeps).
- **sx lock (`sx`)** — sleepable shared/exclusive (reader/writer) lock; readers share, writer
  excludes, and holders may sleep. For long sections that block.
- **rwlock (`rw`)** — reader/writer lock that does *not* allow sleeping while held; cheaper than
  sx for short read-mostly sections.
- **rmlock (`rm`, "read-mostly")** — optimized for *overwhelmingly read* workloads. Readers take
  a **per-CPU** tracker (no shared cache-line write → no contention between readers on different
  CPUs); writers pay a heavy cost to collect all per-CPU readers. Essentially a poor-man's RCU
  with blocking semantics.
- **A documented global lock order** + `WITNESS` (a runtime lock-order verifier) to catch
  deadlocks.

**Why they're fast / key insight.** Match the primitive to the access pattern: short vs. long,
read-mostly vs. balanced, sleepable vs. not. The standout insight is **rmlock/epoch**: for
read-mostly data, make the *reader* touch only per-CPU state so reads don't bounce a shared
cache line — the expense is shoved onto rare writers. Adaptive mutexes avoid sleeping when the
holder is about to release (spin a little) — best of spin and sleep.

**Applicability to oxbow — be skeptical here.** This zoo exists because a monolithic kernel has
*enormous* shared mutable state touched concurrently by every subsystem. oxbow's whole thesis is
the opposite: **state is partitioned per server, the kernel is small, and capabilities mediate
sharing.** Importing six lock types would be importing the disease. Recommended minimal set:
- **Per-CPU data** as the first tool (covers allocator caches, scheduler runqueues, stats) —
  no lock at all on the hot path. This is where most of FreeBSD's "fast lock" wins actually
  come from.
- **One good sleepable lock** (a futex-backed Mutex — you already have futexes) for general
  mutual exclusion in servers and userspace. Rust's `std::sync::Mutex`/`RwLock` over your futex
  is exactly right and you already have it.
- **EBR (crossbeam-epoch)** for read-mostly structures — this *is* the rmlock insight, done
  better. Prefer it over building an rmlock.
- **Adaptive spinning** inside your futex Mutex (spin briefly before syscall-to-sleep) — a small,
  high-value optimization that directly imports the adaptive-mutex idea and cuts IPC/futex
  syscalls under low contention. Worth doing.
- A **lock-order discipline** is still worth keeping even with few locks; a lightweight WITNESS-
  style debug check is cheap insurance, LOW priority.

**Skeptical bottom line:** copy the *taxonomy insight* (per-CPU for read-mostly, adaptive
spin-then-sleep) and the EBR; **do not** port rmlock/sx/rw as distinct primitives. Fewer, well-
chosen primitives is more in keeping with a minimal capability kernel.

**Priority:** HIGH — per-CPU data pattern + adaptive spinning in the futex Mutex + EBR for
read-mostly. LOW — porting the full lock zoo (actively avoid). LOW — WITNESS-style checker
(nice debug insurance later).

---

## 6. Observability — DTrace & hwpmc

**What they are.**
- **DTrace** — dynamic, production-safe instrumentation. Probes (static SDT tracepoints, fbt
  function boundary tracing, pid provider, syscall, profile timer) fire D-language scripts that
  aggregate in-kernel with near-zero overhead when disabled. Lets you ask "what is the system
  doing right now" without rebuilding.
- **hwpmc** — interface to the CPU's hardware **Performance Monitoring Counters** (cycles,
  instructions, cache misses, branch mispredicts, etc.), supporting counting and sampling
  (statistical profiling: periodically sample the PC on counter overflow → flame graphs).

**Why they're valuable / key insight.** You cannot optimize what you cannot measure, and
microbenchmarks lie. FreeBSD's entire perf-engineering culture is "instrument with DTrace/pmc,
find the real hot path, then fix it." Low-disabled-overhead probes mean you measure the *real*
workload in place. hwpmc tells you *why* (cache miss? branch? just cycles?), not just *where*.

**Applicability to oxbow.** You can't and shouldn't port DTrace (huge, deeply monolithic). But
the *capability* is buildable and unusually important for oxbow because **your costs are domain
crossings you can't see without measurement** (IPC round-trips, scheduler latency, the data-
plane ring throughput). Concretely:
- **hwpmc-style PMC access: HIGH value, low effort.** x86 PMCs are a handful of MSRs (IA32_PERF*
  / fixed counters). A small kernel capability that lets an authorized profiler read/program PMCs
  gives you cycles/cache-miss/IPC numbers. Sampling (PMI interrupt on overflow → record PC) gets
  you statistical profiles/flame graphs. This is the single best observability ROI.
- **Static tracepoints (SDT-style) + a per-CPU ring buffer.** Cheap `if (probe_enabled)` hooks at
  key spots (IPC send/recv, syscall entry/exit, sched switch, ring doorbell, allocator slow path)
  that, when enabled, write a timestamped record into a per-CPU lock-free ring drained by a
  userspace tracer over IPC. This is a poor-man's DTrace, very buildable, and exactly tailored to
  measuring IPC/scheduler/ring costs.
- **Key metrics to expose first:** IPC round-trip cycles, syscall count per operation (your notes
  already caught the "1800 syscalls per 100KB read" pathology by counting — formalize that),
  scheduler wakeup→run latency, allocator hit-rate, ring batch sizes. These directly target the
  microkernel tax.

**Priority: HIGH for PMC access + a few static tracepoints with a per-CPU ring.** Build this
*early* — it tells you whether the netmap-ring and per-CPU-allocator work above is actually
paying off, and stops you optimizing the wrong crossing. Full DTrace: don't.

---

## 7. Microkernel-relevant ideas (cross-cutting)

These aren't a single FreeBSD subsystem but the techniques above distilled to "what makes a
*microkernel* fast," since that's oxbow's central challenge.

- **Batching to amortize the domain crossing.** Every netmap, UMA-bucket, and tcache win is the
  same move: do work in batches so the expensive boundary (syscall / IPC / atomic) is paid once
  per N items. For oxbow, *batch everything that crosses the IPC boundary*: packets (rings),
  fs reads (bulk read — you already found the 56-byte-chunk pathology), capability grants, log
  records. **This is the master technique.** Priority HIGH.

- **Shared-memory data plane + control-plane doorbell.** Move bulk data through preestablished
  shared memory (region capability passed once); use the synchronous IPC only as a wakeup. Net
  (netmap), fs (shared read buffer), and the window server (framebuffer) all want this shape.
  You already do it for the framebuffer — generalize the pattern. Priority HIGH.

- **Fast IPC path.** seL4's reputation rests on a register-only, no-allocation, single-copy (or
  zero-copy) IPC fast path with the scheduler-bypass "call/reply" donating the timeslice to the
  callee. Make sure oxbow's IPC fast path: (a) passes small messages in registers, (b) does a
  direct hand-off to the callee without a full reschedule (donate the slice), (c) avoids touching
  the capability table's slow path for the common endpoint. Measure it with the PMC work above.
  Priority HIGH — it's the per-crossing constant that everything else multiplies.

- **Zero-copy via capability transfer.** Instead of copying a buffer across servers, transfer a
  frame capability (move semantics — your notes note handles MOVE on send). This is the
  capability-native zero-copy and is *cleaner/safer* than FreeBSD's refcounted mbuf clusters
  because the type system enforces single-owner transfer. The open issues you noted (no
  frame_unmap, remap panics, recv doesn't expose sender PID) are exactly the plumbing needed to
  make shared-ring zero-copy work — fixing them unblocks both netmap-style rings and bulk fs I/O.
  Priority HIGH (it's the enabler for #2).

- **Per-CPU everything.** Allocator caches, scheduler runqueues, stats counters, trace ring
  buffers — keep them per-CPU to avoid cross-core cache-line contention. You have SMP, so this
  pays off now. Priority HIGH.

---

## Priority summary table

| Technique | Where it lives in oxbow | Priority | One-line justification |
|---|---|---|---|
| netmap-style shared-ring + batched doorbell (net data plane) | servers (net↔driver↔app) + frame caps | **HIGH** | Determines whether networking is microkernel-tax-bound; biggest single lever |
| Per-thread-cache userspace allocator (jemalloc/mimalloc/snmalloc shape) | std/rt | **HIGH** | Every program allocates; pure userspace; adopt snmalloc/mimalloc |
| Per-CPU slab allocator for kernel objects (UMA shape) | kernel | **HIGH** | Cap slots/IPC bufs/TCBs on every hot path; bounded, lock-free |
| Per-CPU runqueues + affinity + work stealing (ULE) | kernel scheduler | **HIGH** | Scalability + cache warmth; fixes any global-runqueue bottleneck |
| EBR (crossbeam-epoch) for read-mostly tables | net server (ARP/route/socket), scoped | **HIGH** | Near-free reads on hottest path; Rust library already exists |
| Adaptive spin-then-sleep in futex Mutex + per-CPU data | rt/servers | **HIGH** | Imports the real "fast lock" wins; cuts futex syscalls |
| In-fs-server LRU page cache + name cache (buffer cache shape) | fs server | **HIGH** | You already hit this pain with /bin loads |
| PMC access + static tracepoints/per-CPU ring (hwpmc/DTrace-lite) | kernel + userspace tracer | **HIGH** | Can't optimize unseen IPC/sched costs; build early |
| Fast IPC path (register-only, slice donation) | kernel | **HIGH** | Per-crossing constant everything multiplies |
| Zero-copy via frame-capability transfer (plumbing: unmap/remap/sender-id) | kernel + rt | **HIGH** | Enabler for shared-ring + bulk fs I/O |
| mbuf headroom + refcounted shared payloads | net server | **MED-HIGH** | Cheap header push/pop + zero-copy fanout; clean in Rust |
| IPC priority/budget donation (vs ULE interactivity scoring) | kernel scheduler | **MED** | Better microkernel fit than autodetected interactivity |
| SO_REUSEPORT_LB | net server socket-cap API | **MED** | Enables multi-worker scaling; not urgent pre-multi-queue |
| Checksum/TSO offload | driver + smoltcp flags | **MED** | Gated on capable driver (virtio-net) |
| Unified buffer cache via shared frames | fs server + VM | **MED** | Elegant in caps; contingent on file-backed mmap |
| RSS / multi-queue per-CPU net slices | driver + net server | **LOW→MED** | Gated on multi-queue NIC hardware |
| FreeBSD TCP fast path port | n/a | **LOW** | You use smoltcp; don't reimplement |
| Full rmlock/sx/rw lock zoo | n/a | **LOW (avoid)** | Monolithic-shared-state disease; EBR+per-CPU instead |
| Superpage-backed UMA kegs | kernel | **LOW** | Needs 2MB page path first |
| Full DTrace port | n/a | **LOW (don't)** | Too monolithic; build PMC + tracepoints instead |

---

*Sources: netmap (Rizzo, USENIX ATC '12) and netmap(4); FreeBSD epoch(9), mbuf(9), UMA design
notes (jeffr-tech); FreeBSD locking(9) / rmlock(9) / sx(9); ULE scheduler docs; hwpmc/DTrace
handbook chapters; crossbeam-epoch / Aaron Turon "Lock-freedom without garbage collection."
Cross-checked against oxbow's current architecture per project memory.*
