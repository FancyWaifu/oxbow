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

### Phase 5 — Real locking (the core of the arc) — STARTED (§69)
- **Per-CPU TSS + per-CPU idle thread — ✅ DONE** (the prerequisites; no locking yet):
  - **Per-CPU TSS** (`gdt.rs`): one `TaskStateSegment` per CPU, each with its own RSP0 (ring-0
    trap/syscall stack) and its own #DF IST stack, plus a distinct TSS descriptor in the GDT
    (a TSS can only be `ltr`'d on one CPU — loading sets the busy bit). The GDT is resized to
    `1 + 4 + 2·MAX_CPUS` entries; the BSP appends all TSS descriptors in `init` and `ltr`s its
    own; each AP `ltr`s its own in `gdt::load_ap(cpu)`. `set_rsp0` now writes the *running*
    CPU's TSS (via `percpu::cpu_index()`), so two cores scheduling at once can't clobber each
    other's RSP0.
  - **Per-CPU idle thread**: `PerCpu` gains `idle_tid` (at `gs:[16]`); the scheduler's
    hardcoded `IDLE`/`unwrap_or(IDLE)`/`current()!=IDLE` sites now go through
    `percpu::idle_tid()`. The BSP idles on TCB 0 (unchanged); each AP registers its own idle
    TCB (`thread::register_running_idle` — adopts the already-running bringup stack, no new
    frame) and sets `current`/`idle_tid` to it. `any_active` excludes every CPU's idle so an
    idling AP isn't mistaken for work.
  - Verified under `-smp 4`: AP comes up on its own TSS + idle TCB, no #GP/#PF/#DF; BSP boots
    fully and the ring-3 path (login, syscalls, context switches with per-CPU RSP0, interleaved
    kbd/mouse) all pass. The AP still just idles IF=0 — it does not yet run the scheduler.
- **Lost-wakeup fix — ✅ DONE (§70).** Implemented the proven sleep/wake protocol from
  Linux (`set_current_state` + `smp_mb` then re-check; `try_to_wake_up`) and OpenBSD/NetBSD
  (`sleep_setup`/`sleep_finish(do_sleep)`, the `slock` interlock "wakeup-before-sleep"
  guarantee). Concretely:
  - Thread `state` moved into its own atomic array (`STATE: [AtomicU8; N]`) — like Linux's
    `task->__state` (`READ_ONCE`/`WRITE_ONCE`) and BSD's `p_stat` — so a waker on another CPU
    and the sleeper never race/tear on it.
  - Two-phase block: `prepare_block()` = set `Blocked` + `SeqCst` fence (Linux
    `set_current_state`); the caller then **re-checks the condition**; `block_current()` sleeps
    **only if still `Blocked`** (OpenBSD `sleep_finish`), so a wake landing in the gap isn't
    lost — it either makes the condition true (caught by the re-check) or flips us `Ready`
    (caught by `block_current`). `cancel_block()` backs out when the condition was already true.
  - `wake()` is now a single atomic CAS `Blocked → Ready` (Linux `try_to_wake_up`'s state CAS),
    idempotent and safe against concurrent wakers.
  - Every wait site reordered to **publish-self → prepare_block → re-check → sleep**: IPC
    send/recv/call (`ipc.rs`), notifications (`notif.rs`), channel send/recv + multi-wait
    `sys_chan_wait`, and pipe read/write (added `unpark_*` so a non-sleeping waiter frees its
    queue slot). For the call→reply rendezvous, `prepare_block` is set **before waking the
    receiver** (it may reply from another CPU the instant it runs).
  - Verified under `-smp 4`: login + fs commands + file reads (all IPC/channel/tty) pass with
    no regression, no faults. Single-core behavior is identical (with IF=0 nothing runs in the
    prepare→sleep gap, so the re-check is a no-op there).
- **Mechanism B — the context-switch handoff — ✅ DONE (§71).** A thread woken on CPU B must
  not be resumed there until it has fully context-switched off CPU A. Implemented OpenBSD's
  single-`SCHED_LOCK` model (chosen over Linux's per-CPU rq locks + `p->on_cpu` spin — one global
  lock serializes everything, far simpler and a better fit for this kernel):
  - A raw spinlock `SCHED_LOCK` (`thread.rs`) is held across the ENTIRE run-queue decision AND
    `context_switch`, and released by whatever thread **resumes** — its own `switch_to` tail, or
    `thread_trampoline` for a freshly spawned thread (`sched_unlock_c`, called from the asm).
    This is exactly Linux's `finish_task_switch` / OpenBSD's "the new thread drops SCHED_LOCK"
    handoff. Because the lock spans the switch, no other CPU can `pick_next` a thread until its
    context is fully saved — so a woken thread is never resumed on two cores at once, WITHOUT a
    separate `on_cpu` flag (the single lock subsumes it).
  - `block_current`/`preempt`/`exit_current`/`yield_now` acquire `SCHED_LOCK`; `switch_to`
    releases it (after the switch, in the resumed thread — or immediately when there's no switch).
    The acquire and release live in different stack frames / different threads, so it's a raw
    lock, not an RAII guard.
  - Discipline keeping it deadlock-free: taken alone (never nested under a per-structure lock —
    wait sites drop their interlock before `block_current`), and always released before the CPU
    returns to IF=1, so the timer IRQ can never fire while a CPU holds it.
  - Verified under `-smp 4`: the handoff runs on every context switch (thousands during boot +
    interactive use); login + fs + pipes + interleaved kbd/mouse all pass, no deadlock, no fault.
    Single-core: `SCHED_LOCK` is uncontended (BSP only), so behavior is unchanged.
- **AP scheduling — ✅ DONE (§72). The payoff: user threads now run on a second core.**
  - **Per-CPU syscall stack:** the `syscall` stub's `CURRENT_KSTACK_TOP` / `USER_RSP` moved into
    `PerCpu` (gs:[24]/gs:[32]); the naked stub reads them gs-relative. No swapgs needed — oxbow
    keeps ONE GS base per CPU across the ring boundary (user can't change it), so `gs:[..]` in the
    stub resolves to THIS CPU's PerCpu even straight out of ring 3.
  - **TCB-allocation race fixed:** `spawn()` now runs under `SCHED_LOCK` and publishes `Ready`
    LAST (after writing the whole TCB), so a scheduler on another core can't grab the same slot
    or pick a half-initialised thread.
  - **AP CPU-feature init:** each AP replays the per-CPU half of `arch::init` — the syscall MSRs
    (EFER.SCE/STAR/LSTAR/SFMask) and SSE (CR0/CR4 + `fninit`) — via `init_ap_cpu()`. (First bug
    found: a user thread `#UD`'d on the AP because CR4.OSFXSR wasn't set there.)
  - **AP runs the scheduler:** `ap_main` starts the AP's LAPIC timer (reusing the BSP-cached
    calibration) and enters `run_idle`; its timer drives `preempt`, pulling Ready threads off the
    shared run queue under `SCHED_LOCK`. Timekeeping (TICKS, the ~1 Hz tick notif, timed-wait
    deadlines) stays BSP-only so the clock doesn't run N× fast; every core preempts.
  - **Verified under `-smp 4`:** serial shows `[smp] AP cpu 1 is now running user threads` — real
    user work on the second core — and login + fs + pipes + interleaved kbd/mouse all pass with no
    fault, no deadlock, no corruption. This is the first contended exercise of §70/§71/§72.
- **Lock-ordering audit — ✅ DONE (§73). Result: the lock graph is acyclic → deadlock-free.**
  Enumerated all 15 locks and every nested acquisition (lock held while another is taken).
  **Canonical global order (acquire high→low; never the reverse):**
  ```
  ENDPOINTS > PROCESSES > REPLIES > REGIONS > SCHED_LOCK > BINDINGS
            > { CONNS, PIPES, POOL(notif), RNG, IMAGES, MEMORY, FRAMES, BUMP(pmm) }  [leaves]
            > SERIAL  [absolute bottom — pure I/O, acquires nothing]
  ```
  The only multi-lock holders and their (forward-only) edges:
  - `ipc::recv` holds ENDPOINTS across `transfer_into` (→PROCESSES) and `mint_reply`
    (→REPLIES, then →PROCESSES) — each sub-lock acquired+released, never two at once.
  - `proc::kill` holds PROCESSES across `reply_abandon` (→REPLIES); drops PROCESSES before all
    mem/notif calls (its own LOCK RULE comment).
  - `shm::{create,map,free}` hold REGIONS across pmm/vm (→BUMP). Wide but acyclic (REGIONS is a
    high coarse lock); left as-is for region-atomicity.
  - `irq::fire` previously held BINDINGS across `notif::signal` (→POOL); §73 tightened it to copy
    the binding out and drop BINDINGS first.
  - `thread::switch_to`/`spawn` hold SCHED_LOCK across only `announce`/the §72 proof `println!`
    (→SERIAL); never any data lock.
  - The mm leaves (pmm/mem) only ever release before calling each other (`frame_unmap`: FRAMES
    released before pmm), so they never nest.
  **Why cross-CPU spinning can't deadlock:** every kernel critical section runs with **IF=0**
  (SFMask masks IF on `syscall`; IRQ gates clear it), so a core NEVER takes an interrupt while
  holding a lock — the only contention is another core spinning, which always makes progress
  because the holder needs nothing the spinner holds (acyclic order). This is the design
  invariant that lets oxbow use plain `spin::Mutex` everywhere without IRQ-safe variants.
- ✅ The **"never hold a lock across `block_current`"** rule holds at every wait site (§70):
  each drops its interlock before `block_current` (which takes SCHED_LOCK alone).
- **All cores + the `on_cpu` fix — ✅ DONE (§74).** `bring_up_all` now starts every available
  AP (sequentially, capped at `MAX_CPUS`), so all cores schedule. Bringing up >1 AP exposed a
  real double-run bug the single-AP config hid: the §70 lost-wakeup window leaves a thread
  `Ready` while it's STILL executing toward `block_current` on its core (a waker CASed it `Ready`
  before it saved its context), and another core's `pick_next` would resume it from a STALE saved
  context — the same thread on two cores (caught a `#GP` with a corrupt rip + a direct double-run
  detector). This is exactly Linux's `p->on_cpu` problem; the SCHED_LOCK handoff does NOT subsume
  it because the wake is lock-free. **Fix:** a per-TCB `RUNNING_ON` (`on_cpu`) flag maintained in
  `switch_to` under SCHED_LOCK; `pick_next` skips `Ready` threads still on a core, so a
  woken-but-still-running thread isn't pickable until it has truly switched off (which clears the
  flag under SCHED_LOCK, after the context save). Also fixed: `exit_current` now sets `Exited`
  under SCHED_LOCK (was a separate stack-reuse race).
- **⚠️ OPEN BUG — multi-AP hang; capped to 1 AP for now.** The `on_cpu` + exit fixes made the
  desktop reliable at **1 AP / 2 cores** (verified 6/6), but at **≥3 cores** (2+ APs) heavy
  boot-time concurrency still hits a residual hang: one core spins on a lock while another is stuck
  in the page-fault handler (kernel state corrupted) — the desktop never composites (the user's
  "stuck on the gradient"). So `bring_up_all` is capped at `MAX_APS_TO_START = 1` in `smp.rs` until
  this is root-caused; `-smp 4` still works, the extra cores just stay parked. (Separately,
  `[fsd] FATAL: could not mount ext2` is a PRE-EXISTING flaky virtio-blk/ext2 issue — it occurs at
  `-smp 1` and as far back as §71, does NOT block the desktop, and is unrelated to the SMP work.)
  Next debugging step: the multi-AP corruption is likely another scheduler/IPC race the 4-core
  window widens; reproduce with a fault-detail dump (cr2/rip) under `-smp 4` with the cap lifted.
- Open (minor, non-blocking): `TCB.wake_at` is a plain `u64` read cross-core in `wake_expired`
  (benign torn-read on x86, but should be `AtomicU64` for strictness); per-CPU run queues +
  work-stealing if contention ever shows (today one global run queue under SCHED_LOCK is fine).
- The frame allocator + page-table edits are locked (BUMP); **TLB shootdown** is Phase 6 (only
  needed once a single process is multi-threaded — today every process is single-threaded).

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
