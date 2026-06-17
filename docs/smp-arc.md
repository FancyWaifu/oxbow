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
- **2a — LAPIC enable on the BSP — ✅ DONE (§69).** `arch::lapic::enable()` maps the LAPIC
  MMIO page into the kernel higher half (`map_mmio_kernel_4k_in`, uncacheable — the HHDM
  does not cover MMIO holes), software-enables the LAPIC (SVR), and sets **virtual-wire
  mode** (LINT0=ExtINT, LINT1=NMI) so the 8259 PIC's interrupts still pass through and the
  PIT keeps driving the scheduler. A 0xFF spurious-vector handler is installed. Verified
  under `-smp 4`: LAPIC enabled (id 0), full login + `ls` runs — scheduler + IPC + input
  unaffected.
- **2b — LAPIC timer — ✅ DONE (§69).** `lapic::start_timer(vector, hz)` calibrates the
  LAPIC timer against **PIT channel 2** (polled, no IRQs) — counts LAPIC ticks during one
  10 ms PIT countdown — then runs it PERIODIC on local vector 0x30. `kmain` now arms the
  LAPIC timer instead of the PIT (IRQ0 stays masked); a `lapic_timer` IDT handler does the
  same TICKS++/wake_expired/preempt work as the old PIT handler but EOI's the LAPIC.
  Keyboard/mouse/serial still reach the CPU via the PIC's virtual-wire LINT0. The calibrated
  count is cached so APs can start their own timer without re-measuring. Verified under
  `-smp 4`: full desktop + login + `ls`, and sysmon's uptime advanced exactly 10 s over 10 s
  wall-clock (calibration accurate). LAPIC enable + timer moved to `kmain_stage2` (after the
  CR3 switch) so the MMIO mapping lives in the kernel PML4 that persists + that user spaces
  copy their higher half from.
- **2c — IOAPIC — ✅ DONE (§69).** `arch::ioapic` maps the IOAPIC MMIO (0xFEC00000) and
  programs redirection entries for the **ISA IRQs the system uses** — keyboard (GSI1→0x21),
  serial (GSI4→0x24), mouse (GSI12→0x2C) — delivering to the BSP's LAPIC, edge/active-high.
  Those handlers now mask via the IOAPIC + EOI the LAPIC (not the PIC), and `irq::ack` re-arms
  the IOAPIC for routed lines. The PIC lines stay masked, so each IRQ arrives once. **PCI IRQs
  (the NIC) deliberately stay on the PIC's virtual wire** — routing PCI INTx→GSI needs the ACPI
  `_PRT` and is deferred (a clean hybrid; networking is unaffected). Verified under `-smp 4`:
  login (keyboard) + cursor drag (mouse) work through the IOAPIC, DHCP still leases (PIC NIC
  path untouched), full desktop unaffected.
- (legacy notes) Add the **LAPIC timer** (one-shot or periodic, calibrated against the PIT once) as the
  per-CPU scheduler tick — replacing the shared PIT.
- Risk: medium. Getting IOAPIC redirection + EOI right; keep the PIT path until the LAPIC
  timer is proven, then retire it.

### Phase 3 — AP bringup → idle — ✅ DONE (§69, one AP)
- Used the **Limine MP feature** (no hand-rolled trampoline / INIT-SIPI-SIPI): Limine already
  started + parked every AP, so bringup is just `cpu.goto_address.write(ap_entry)`. New
  `kernel/src/smp.rs`.
- Limine jumps the AP to `ap_entry` on the **bootloader's page tables + a temporary stack**, so
  `ap_entry` reads the few values it needs (its index from `cpu.extra`, its stack, the kernel
  PML4) and then **in one asm block** switches RSP onto a dedicated per-CPU `.bss` stack and CR3
  onto the kernel PML4, then tail-calls `ap_main` — mirroring the BSP's `switch_address_space`.
  Both the new stack (HHDM) and kernel `.text` are mapped under both page tables, so the switch
  is seamless.
- `ap_main`: load the shared GDT/IDT + reload segments (NO `ltr` — one shared TSS, single busy
  owner; `gdt::load_ap`), claim per-CPU state via `percpu::init(index)` (its own GS base),
  `lapic::enable()` (reuses the BSP's LAPIC MMIO mapping), publish `AP_ONLINE`, then **`hlt`
  forever with IF=0**. The BSP bounded-spins on `AP_ONLINE` and logs "AP k online" (the AP never
  touches the console — not lock-safe yet).
- **Deliberately minimal & safe:** the AP runs NO scheduler/IPC/allocator code and takes NO
  interrupts (its LAPIC timer is left unstarted; device IRQs route to the BSP). It shares no
  mutable state with the BSP, so no locking is needed yet — that's Phase 5.
- **Verified** under `-smp 4`: serial shows `[smp] AP 1 online (lapic_id=1) — parked in idle on
  its own core`, and the BSP boots through to a working shell (login + ls + interleaved
  kbd/mouse all pass) — the spin-wait doesn't stall the BSP.
- TODO (Phase 5): per-CPU idle thread + TSS, then let the AP actually run the scheduler.

### Phase 4 — Per-CPU state — STARTED (§69)
- **`CURRENT` (running tid) → per-CPU — ✅ DONE.** New `kernel/src/percpu.rs`: a `PerCpu`
  struct ({cpu_index, current}) reached through the **GS base**. oxbow uses no `swapgs`
  (user never touches GS, CR4.FSGSBASE off), so the kernel just points `IA32_GS_BASE` at this
  CPU's `PerCpu` and accesses fields via the `gs:` prefix (`gs:[0]`=index, `gs:[8]`=current) —
  fast, no rdmsr on the hot path, valid in interrupt/syscall context. `thread::current()` and
  the two `CURRENT` writes (init, `switch_to`) now funnel through `percpu`. The BSP calls
  `percpu::init(0)` at the top of `kmain_stage2` before anything reads `current()`. `PERCPU`
  lives in the kernel higher half (PML4 256..512), which every user space copies, so `gs:`
  works from any process's interrupt context. **Verified under `-smp 4`:** login + multiple
  commands run, scheduler/IPC/syscalls all use the per-CPU current — identical on one core.
- **Still TODO (with Phase 3):** per-CPU **idle thread** (TCB 0 is global today), and the
  run queue. Start with **one global run queue** under a spinlock (simplest, correct); move to
  per-CPU queues + work-stealing later only if contention shows up. Each AP will call
  `percpu::init(k)` at bringup to claim its own slot + GS base.

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
