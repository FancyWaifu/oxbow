# SMP Arc — scope (AP bringup → per-CPU → locking)

Status: **scoping only** (not started). This is the kernel's single largest and
highest-risk arc. It overturns the foundational v0/v1 invariant that the kernel is
single-core and **never preemptible** (`IF=0` in every kernel path), which today makes
every syscall atomic with respect to other threads. SMP removes that guarantee, so every
shared structure needs real mutual exclusion and every "this can't race" assumption has to
be re-audited.

## Where we are today (the invariant SMP breaks)

- **One CPU.** QEMU is launched with no `-smp`; only the BSP runs. `kernel/src/thread.rs`
  is a round-robin scheduler over a global thread table with a single `static mut CURRENT`.
- **Non-preemptible kernel.** All IDT gates clear `IF`; preemption only happens at ring-3
  boundaries / `sti;hlt` idle. So a syscall runs start-to-finish with no other thread
  interleaving — this is *why* `park_recv`+`block_current` (and the new `sys_chan_wait`)
  are lost-wakeup-safe without extra fences (see ABI §63).
- **`spin::Mutex` already wraps the shared pools** (`CONNS`, `ENDPOINTS`, `notif`, `proc`,
  `shm`, `pipe`, `irq`, `image`, `rng`) — but on one core these locks are never actually
  contended; they're effectively `RefCell`s. Under SMP they become real and the **lock
  ordering / never-hold-across-block rules become load-bearing**, not advisory.
- **Timer = PIC/PIT IRQ0** (`idt.rs` `TIMER_VECTOR=0x20`, `TICKS`). One timer, one CPU.

## Phases

### Phase 1 — Topology discovery — ✅ DONE (§69)
- **Used Limine MP instead of hand-parsing the MADT.** The `limine` crate's `MpRequest`
  has Limine parse the ACPI MADT, start every AP, and PARK it — handing us `mp.cpus()`
  (each `Cpu { id, lapic_id, goto_address, extra }`) and `mp.bsp_lapic_id()`. This collapses
  the old Phase 1 (MADT parse) AND most of Phase 3 (AP trampoline / INIT-SIPI-SIPI): to
  start an AP we just write its `goto_address` and Limine launches it at our Rust entry with
  its own 64 KiB stack.
- Done: `MP_REQUEST` static + boot enumeration. Verified under `-smp 4`: "[smp] 4 CPU(s);
  BSP lapic_id=0" + each AP listed (parked). No behavior change — APs stay parked.
- `-smp 4` added to the justfile QEMU flags so there are cores to bring up.
- Still TODO from the old Phase 1: we may still want the **IOAPIC base** from the MADT for
  Phase 2 IRQ routing (Limine gives the RSDP via `RsdpRequest` if we need to walk it).

### Phase 2 — LAPIC + IOAPIC enablement
- Map the LAPIC MMIO page; enable it on the BSP. Switch IRQ routing from the legacy 8259
  PICs to the **IOAPIC** (mask the PICs fully, route IRQ1/IRQ12/serial/PCI lines through
  IOAPIC redirection entries to the BSP's LAPIC for now).
- Add the **LAPIC timer** (one-shot or periodic, calibrated against the PIT once) as the
  per-CPU scheduler tick — replacing the shared PIT.
- Risk: medium. Getting IOAPIC redirection + EOI right; keep the PIT path until the LAPIC
  timer is proven, then retire it.

### Phase 3 — AP trampoline + bringup
- Place a 16-bit **trampoline** at a low physical page: real mode → enable protected → load
  our existing GDT/page tables → long mode → jump to a Rust `ap_main`. (Mirror Limine's own
  SMP handoff if we use Limine's SMP request — Limine can start APs for us and hand each a
  stack + entry, which removes most of the hand-written trampoline. **Recommended:** use the
  Limine SMP feature first; hand-roll INIT-SIPI-SIPI only if we outgrow it.)
- Each AP: load IDT, enable its LAPIC + LAPIC timer, set up per-CPU state (Phase 4), then
  enter the scheduler idle loop.
- Deliverable: all N cores reach `ap_main` and idle; boot log "AP k online".
- Risk: high. Trampoline/paging bugs triple-fault silently. Bring up **one** AP first.

### Phase 4 — Per-CPU state
- Replace global CPU-local singletons with a per-CPU array indexed by LAPIC id, reachable
  via `GS` base (`swapgs` on kernel entry, or `KERNEL_GS_BASE`):
  - `CURRENT` (running tid) → **per-CPU** (the biggest single change).
  - the idle thread → one per CPU.
  - LAPIC timer tick counters.
- Scheduler: a run queue. Start with **one global run queue** under a spinlock (simplest,
  correct); move to per-CPU queues + work-stealing later only if contention shows up.
- Risk: high. Every `thread::current()` / `current_proc()` caller must go through the
  per-CPU accessor. Context switch must save/restore to the right CPU's slot.

### Phase 5 — Real locking (the core of the arc)
- Audit every `spin::Mutex` user for **lock ordering** (define a global order, e.g.
  proc < cap-table < channel < endpoint < notif < frame-alloc) and never acquire out of
  order → deadlock freedom.
- Enforce the existing **"never hold a lock across `block_current`"** rule mechanically;
  it's already the convention (ipc.rs LOCK RULE) but SMP makes a violation a real hang.
- IRQs while holding a kernel lock: either keep `IF=0` in lock critical sections (simplest:
  a CPU never takes an IRQ while holding a spinlock) or use IRQ-safe locks. Start with the
  former.
- The frame allocator + page-table edits need locking; **TLB shootdown** (Phase 6).
- Re-audit the §63 lost-wakeup argument: with true concurrency a sender on CPU B can run
  between a waiter's check and `block_current` on CPU A. Fix: hold the channel lock across
  the readiness check **and** the state transition to Blocked (deposit-before-release), or
  add a per-thread "wake pending" flag that `block_current` consults. **This is the single
  most subtle correctness item in the arc.**
- Risk: very high. This is where races and deadlocks live.

### Phase 6 — TLB shootdown + cross-CPU signalling
- An `unmap`/`protect` on one CPU must invalidate other CPUs' TLBs: send an **IPI** to the
  CPUs running threads of the affected address space; they `invlpg` and ack.
- Reschedule IPI: waking a thread that belongs on another CPU pokes that CPU.
- Risk: high. Easy to miss a shootdown → stale translations → memory corruption.

## Recommended order & checkpoints
1. Phases 1–2 on the BSP only (no APs yet) — IOAPIC + LAPIC timer driving the existing
   single-core scheduler. **Checkpoint: boots and runs exactly as before, on the LAPIC timer.**
2. Phase 4 per-CPU plumbing while still single-core (CURRENT via GS) — **Checkpoint: no
   behavior change.**
3. Phase 3 bring up **one** AP into an idle loop (no work yet) — **Checkpoint: 2 cores, AP idles.**
4. Phase 5 global-run-queue + locking audit; let the AP actually run threads — **Checkpoint:
   a busy-loop test program runs on the AP while the shell runs on the BSP.**
5. Phase 6 TLB shootdown — **Checkpoint: a multi-threaded mmap/munmap stress test is stable.**
6. Only then: per-CPU run queues / work-stealing / affinity (optimization, not correctness).

## Why this helps the desktop (the original motivation)
Today a continuously-animating client (the rings demo) saturates the one core; the
compositor and input still get slices (event-driven now, §63) but compete for CPU. With
SMP the client renders on one core while the compositor + input run on another → the
animation no longer steals from interactivity. SMP is the right tool **for CPU-bound
parallel work**; it is *not* a substitute for the §63 spin fix (a busy-poll would just
saturate its own core).

## Rough size
Weeks, not days. Phases 1–4 are mechanical-but-careful; Phase 5 is the hard correctness
work; Phase 6 is the last sharp edge. Each phase boots and is verified before the next.
