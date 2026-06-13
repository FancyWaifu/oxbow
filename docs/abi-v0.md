# oxbow ABI — v0

**Status:** normative for v0 ("first light"). **Scope:** exactly what is needed to boot, hand-build one user server from a Limine module, and complete one synchronous IPC roundtrip ending in `PONG` on serial. Everything else is deferred (§8).

Both the kernel and userland depend on the `oxbow-abi` crate, which is the single source of truth for syscall numbers, rights bits, error codes, and message layout. All types are `#[repr(C)]`, little-endian, on `x86_64-unknown-none`. `oxbow-abi` exports `pub const ABI_VERSION: u32 = 0;`.

---

## 1. Design laws (normative)

These invariants MUST hold in every version of oxbow, starting now. A change that violates one is a bug, not a feature.

- **L1 — Zero ambient authority.** Every syscall MUST operate on a handle the caller already holds (sole exceptions: `sys_exit`, which acts only on the calling thread itself, and the syscall mechanism itself). There MUST be no syscall that grants access to an object the caller could not already name via a handle.
- **L2 — Handles are unforgeable and process-local.** A handle is an index into the calling process's private handle table. The integer value carries no meaning in any other process. Userspace MUST NOT be able to fabricate a valid handle by guessing integers: the kernel MUST validate index, occupancy, expected object type, and rights on every use.
- **L3 — No global namespace.** The kernel MUST NOT provide any "open by name", "lookup by id", or enumerate-objects facility. A process is born holding ONLY the handles its parent (in v0: the kernel acting as parent) explicitly granted, and can acquire new handles only by receiving them in messages or deriving them by attenuation.
- **L4 — W^X, always.** The kernel mapper MUST NOT ever create a mapping that is simultaneously writable and executable, in kernel or user space, including transiently. The ELF loader MUST refuse (panic at boot in v0) any segment requesting `W|X`. All non-executable mappings MUST be NX.
- **L5 — Attenuation only, never amplification.** Every operation that produces a handle from an existing handle MUST produce rights that are a subset (⊆, equality allowed) of the source handle's rights. There is exactly one attenuation primitive (`sys_attenuate`); no syscall may add rights.
- **L6 — Memory accountability.** The kernel MUST NOT allocate memory on behalf of userspace except as authorized by a capability the caller holds. *v0 simplification:* kernel objects come from small fixed static pools sized at compile time and exhaustion returns `E_NO_MEM`; the accountable-untyped-memory model (caller-supplied memory capabilities, seL4-style retyping) is the v1 path and MUST NOT be precluded by v0 interfaces.
- **L7 — Rendezvous IPC, no kernel buffering.** Endpoint IPC is synchronous: the kernel copies a message directly from sender to receiver only when both are present, and MUST NOT queue message payloads in kernel memory.

---

## 2. Kernel object types (v0)

v0 defines exactly **three** object types. There are deliberately **no** Process, Thread, AddressSpace/VSpace, or Untyped objects in v0: the kernel hand-builds the single server process at boot (§7), so nothing in userspace needs to name those objects yet. They become objects in v1 without ABI breakage (new type tags, new syscall numbers).

Objects are reference-counted by handle-table entries; an object is destroyed when its last handle is closed. *v0 simplification:* objects live in static pools and are never reused after destruction.

### 2.1 Endpoint
A synchronous rendezvous point for IPC. Holds at most a wait-queue of blocked senders or blocked receivers (never payloads, per L7).

- **Operations:** `sys_send`, `sys_call` (require `R_SEND`); `sys_recv` (requires `R_RECV`); `sys_attenuate`, `sys_close`.
- **Rights:** `R_SEND`, `R_RECV`, plus generic `R_GRANT`, `R_ATTENUATE`.

### 2.2 Reply
A one-shot capability to answer a pending `sys_call`. Created **only** by the kernel during a call rendezvous and delivered to the receiver by `sys_recv`. Cannot be created, duplicated, attenuated, or transferred in v0.

- **Operations:** `sys_reply` (consumes it; the slot is freed automatically); `sys_close` (discards it — the blocked caller is unblocked with `E_GONE`).
- **Rights:** implicit single-use send. Its rights word is `0`; it carries neither `R_GRANT` nor `R_ATTENUATE`. *(Forward path: v1 may make replies transferable for proxying.)*

### 2.3 Console
The serial output device, as a capability. Exists so that even debug output obeys L1 — there is no handle-free "print" syscall.

- **Operations:** `sys_console_write` (requires `R_WRITE`); `sys_attenuate`, `sys_close`.
- **Rights:** `R_WRITE` (object-specific), plus generic `R_GRANT`, `R_ATTENUATE`.

---

## 3. Handle model

### 3.1 Table
Each process owns one flat handle table: `[HandleEntry; 64]` in v0 (`oxbow-abi: HANDLE_TABLE_SIZE = 64`). An entry is `{ object_ref, type_tag, rights: u32 }`. This is explicitly **not** a CNode/CSpace tree and never will be exposed as one; growth path is simply a larger/dynamic flat table.

```rust
// oxbow-abi
pub type Handle = u32;
pub const HANDLE_NULL: Handle = 0;   // index 0 is permanently unoccupied
```

- A handle is an opaque `u32` index. Index `0` is reserved invalid; valid handles are `1..64`.
- **Allocation:** kernel picks the lowest free index ≥ 1. **Freeing:** `sys_close`, or implicitly when a Reply handle is consumed by `sys_reply`. Table full → `E_NO_SLOTS`.
- Handles do not carry generation counters in v0 (single-threaded processes make use-after-close a self-inflicted, process-local bug). *(Forward path: 32-bit generation in the upper bits of a 64-bit handle.)*

### 3.2 Rights bitflags

```rust
// oxbow-abi — bits 0..16 generic, bits 16..32 object-specific
pub const R_SEND:      u32 = 1 << 0;  // may send / call on an Endpoint
pub const R_RECV:      u32 = 1 << 1;  // may recv on an Endpoint
pub const R_GRANT:     u32 = 1 << 2;  // handle may be transferred in a message
pub const R_ATTENUATE: u32 = 1 << 3;  // handle may be the source of sys_attenuate
pub const R_WRITE:     u32 = 1 << 16; // Console: may write
```

### 3.3 Attenuation (the one and only derivation primitive)
`sys_attenuate(src, new_rights)` creates a **new** handle in the caller's table referring to the **same object**, with rights exactly `new_rights`.

- Requires `R_ATTENUATE` on `src`; otherwise `E_RIGHTS`.
- `new_rights & src.rights == new_rights` MUST hold (subset, equality allowed — so attenuate-to-equal doubles as `dup`); otherwise `E_RIGHTS`.
- `src` is unaffected. Dropping `R_ATTENUATE` in `new_rights` makes the derived handle a leaf that cannot be further derived. A pledge/unveil analog is therefore: attenuate what you keep, close what you don't.

### 3.4 Handle transfer in messages
A message may carry up to `MSG_HANDLES = 4` handles (§5).

- Each transferred handle MUST carry `R_GRANT` in the **sender's** table, else the send fails with `E_RIGHTS` before any rendezvous side effects.
- Transfer is a **copy**: the receiver gets a fresh slot referring to the same object with **rights identical to the sender's handle** (per L5 this is ⊆, with equality). The sender **retains** its handle. To hand over something weaker, attenuate first, send the derived handle, close it.
- If the receiver's table cannot hold all transferred handles, delivery is aborted atomically: the sender's syscall returns `E_NO_SLOTS`, the receiver stays blocked waiting for the next sender, and no partial transfer occurs.

---

## 4. Syscall surface (v0)

### 4.1 Calling convention
- Instruction: `syscall` (SYSCALL/SYSRET; `rcx` and `r11` are clobbered by hardware).
- Syscall number in `rax`. Arguments in `rdi, rsi, rdx, r10, r8, r9` (System V order with `r10` replacing `rcx`).
- **Primary return** in `rax`: `0` = `OK`, nonzero = error code (§6). **Secondary return** (a newly allocated `Handle`, when the syscall produces one) in `rdx`; `rdx` is `HANDLE_NULL` when there is nothing to return. All other registers are preserved.
- Received message payloads are not passed in registers: send/recv/call/reply take a pointer to a user-memory `MsgBuf` (§5) and the kernel copies through it. Any user pointer that is unmapped, not user-accessible, or misaligned (8-byte) yields `E_FAULT`.
- An unknown syscall number returns `E_NOSYS`.

### 4.2 Process bootstrapping stance
v0 has **no** spawn/map/exec syscalls. The kernel hand-builds the first (and only) process from the Limine module at boot (§7). The syscalls below are the complete set available to that running server. *(Forward path: v1 adds Process/VSpace objects and `sys_map`/`sys_spawn` operating on them; numbers 8+ are reserved for this.)*

### 4.3 The eight syscalls

| # | Name | Signature (`oxbow-abi` types) |
|---|------|-------------------------------|
| 0 | `sys_send` | `fn(ep: Handle, msg: *const MsgBuf) -> SysResult` |
| 1 | `sys_recv` | `fn(ep: Handle, msg: *mut MsgBuf) -> SysResult<Handle /* reply, in rdx */>` |
| 2 | `sys_call` | `fn(ep: Handle, msg: *mut MsgBuf) -> SysResult` |
| 3 | `sys_reply` | `fn(reply: Handle, msg: *const MsgBuf) -> SysResult` |
| 4 | `sys_attenuate` | `fn(src: Handle, new_rights: u32) -> SysResult<Handle /* in rdx */>` |
| 5 | `sys_close` | `fn(h: Handle) -> SysResult` |
| 6 | `sys_console_write` | `fn(con: Handle, buf: *const u8, len: usize) -> SysResult` |
| 7 | `sys_exit` | `fn(code: u64) -> !` |

**0 — `sys_send(ep, msg)`** *(args: rdi=ep, rsi=msg)*
Requires Endpoint type and `R_SEND`. Validates `msg` (`E_FAULT`, `E_MSG` if counts exceed limits, `E_RIGHTS` if any transferred handle lacks `R_GRANT`), then blocks until a receiver rendezvouses; the kernel copies the message and handles, and both sides return. One-way: no Reply object is created (receiver sees `HANDLE_NULL` reply). Errors: `E_BAD_HANDLE`, `E_BAD_TYPE`, `E_RIGHTS`, `E_FAULT`, `E_MSG`, `E_NO_SLOTS` (receiver table full), `E_GONE` (endpoint destroyed while blocked).

**1 — `sys_recv(ep, msg)`** *(rdi=ep, rsi=msg; returns reply handle in rdx)*
Requires Endpoint type and `R_RECV`. Blocks until a sender rendezvouses. On success the kernel has written the message into `*msg`, allocated receiver-side slots for any transferred handles (writing the new indices into `msg.handles`), and — iff the sender used `sys_call` — allocated a one-shot **Reply** handle, returned in `rdx` (`HANDLE_NULL` for plain sends). Errors: `E_BAD_HANDLE`, `E_BAD_TYPE`, `E_RIGHTS`, `E_FAULT`, `E_NO_SLOTS`, `E_GONE`.

**2 — `sys_call(ep, msg)`** *(rdi=ep, rsi=msg)*
Requires Endpoint type and `R_SEND`. Atomically: send `*msg` as in `sys_send`, then block until the matching `sys_reply`; the reply message is written **into the same buffer**, overwriting it (including `msg.handles` with caller-side fresh indices for any handles the replier transferred). Errors: those of `sys_send`, plus `E_GONE` if the replier closes the Reply handle without replying or the endpoint dies.

**3 — `sys_reply(reply, msg)`** *(rdi=reply, rsi=msg)*
Requires Reply type (the handle's existence is the authority; no rights bits checked). Validates `msg` like `sys_send`, copies it to the blocked caller, unblocks it, and **consumes** the Reply handle (its slot is freed on the success path; on validation errors `E_FAULT`/`E_MSG`/`E_RIGHTS` the handle is NOT consumed so the server can retry). Never blocks (the caller is by construction already waiting). Errors: `E_BAD_HANDLE`, `E_BAD_TYPE`, `E_FAULT`, `E_MSG`, `E_RIGHTS`, `E_NO_SLOTS` (caller's table full; Reply handle is consumed and the caller unblocks with `E_NO_SLOTS`).

**4 — `sys_attenuate(src, new_rights)`** *(rdi=src, rsi=new_rights; new handle in rdx)*
Semantics in §3.3. Errors: `E_BAD_HANDLE`, `E_BAD_TYPE` (Reply objects are not attenuable), `E_RIGHTS` (missing `R_ATTENUATE`, or `new_rights` not a subset), `E_NO_SLOTS`.

**5 — `sys_close(h)`** *(rdi=h)*
Frees slot `h`; decrements the object refcount, destroying the object at zero. Destroying an Endpoint unblocks all waiters with `E_GONE`; closing a Reply handle unblocks its caller with `E_GONE`. Errors: `E_BAD_HANDLE`.

**6 — `sys_console_write(con, buf, len)`** *(rdi=con, rsi=buf, rdx=len)*
Requires Console type and `R_WRITE`. Writes `len` bytes (`len <= 1024`, else `E_MSG`) from user memory to the serial console, synchronously. Errors: `E_BAD_HANDLE`, `E_BAD_TYPE`, `E_RIGHTS`, `E_FAULT`, `E_MSG`.

**7 — `sys_exit(code)`** *(rdi=code)* — does not return.
Terminates the calling process (its one thread), closing its whole handle table (with the unblocking effects of `sys_close`). The handle-free exception to L1, noted there. In v0 the kernel logs the exit code and halts.

---

## 5. Message format

```rust
// oxbow-abi
pub const MSG_DATA_WORDS: usize = 8;  // 64 bytes inline payload
pub const MSG_HANDLES:    usize = 4;

#[repr(C)]
pub struct MsgBuf {
    pub tag: u64,                        // user-defined label; kernel never interprets
    pub data_len: u32,                   // valid words in `data`, 0..=MSG_DATA_WORDS
    pub handle_count: u32,               // valid slots in `handles`, 0..=MSG_HANDLES
    pub data: [u64; MSG_DATA_WORDS],
    pub handles: [Handle; MSG_HANDLES],  // sender: handles to transfer (each needs R_GRANT)
                                         // receiver: kernel-written fresh indices
}                                        // size: 104 bytes, 8-byte aligned
```

- v0 messages are **fixed-size, inline only**: at most 8 data words and 4 handles per message, no out-of-line memory, no grants of memory ranges. `data_len`/`handle_count` exceeding the limits → `E_MSG`. *(Forward path: shared-memory VMOs for bulk data; the struct layout is stable.)*
- Copy semantics: at rendezvous the kernel copies `tag`, `data_len`, `handle_count`, the first `data_len` words of `data`, and performs handle transfer per §3.4, writing receiver-local indices into the receiver's `handles[0..handle_count]`. Unused trailing words/slots in the receiver buffer are left unmodified.

---

## 6. Error codes

Returned in `rax`; values are **stable forever** (append-only enum).

```rust
// oxbow-abi
#[repr(u64)]
pub enum SysError {
    // 0 is OK (not in this enum); SysResult = Result<(), SysError> over rax
    BadHandle   = 1,  // index out of range or slot empty
    BadType     = 2,  // object is not the type this syscall expects
    Rights      = 3,  // handle lacks required right; attenuation not a subset
    Fault       = 4,  // bad user pointer (unmapped / not user / misaligned)
    Msg         = 5,  // message exceeds MSG_* limits or len too large
    NoSlots     = 6,  // a handle table is full
    NoMem       = 7,  // kernel object pool exhausted (see L6)
    Gone        = 8,  // peer or object destroyed while blocked / reply abandoned
    WouldBlock  = 9,  // reserved: non-blocking variants are v1; never returned in v0
    Nosys       = 10, // unknown syscall number
}
```

---

## 7. The v0 PONG roundtrip (acceptance test)

This trace is normative. v0 is **done** when this exact sequence works under QEMU.

**Well-known boot handles** (`oxbow-abi`): `BOOT_EP: Handle = 1`, `BOOT_CONSOLE: Handle = 2`. **Protocol tags** (`oxbow-abi`): `TAG_PING: u64 = 0x474E4950` (`"PING"`), `TAG_PONG: u64 = 0x474E4F50` (`"PONG"`).

**Kernel boot (no syscalls involved):**
1. Limine loads the kernel and one module, `server.elf`, listed in `limine.conf`. Kernel brings up serial, GDT/IDT/TSS, physical allocator, and its own page tables (kernel mappings obey L4).
2. Kernel creates Endpoint `EP0` and Console `CON0` from the static pools. The kernel itself logically holds the `R_RECV` side of `EP0`.
3. Kernel hand-builds process P1 from the module: parses the ELF (v0 accepts `ET_EXEC`, x86_64, static, no relocations, no TLS; any `W|X` segment → boot panic per L4), maps `PT_LOAD` segments with exact W^X-clean permissions (text RX, rodata R+NX, data/bss RW+NX), maps a 64 KiB stack ending at `0x0000_7FFF_FFFF_0000` (RW+NX, guard page below).
4. Kernel populates P1's handle table: slot **1** = `EP0` with rights `R_SEND | R_ATTENUATE` (no `R_RECV`, no `R_GRANT` — the server cannot impersonate or leak the kernel side); slot **2** = `CON0` with `R_WRITE | R_ATTENUATE`. Slot 0 is null; all others empty.
5. Kernel enters user mode at `e_entry` (`oxbow-rt`'s `_start`) with `rsp` at stack top. The kernel's boot thread then performs a kernel-internal receive on `EP0` and blocks (it is the synchronous "echo parent" for v0; in v1 this side belongs to another user process).

**Server side (`oxbow-rt` + server crate):**

6. Server builds `MsgBuf { tag: TAG_PING, data_len: 0, handle_count: 0, .. }` and invokes **`sys_call(BOOT_EP, &mut msg)`** — exercising user→kernel entry via `syscall`, handle lookup, type check (Endpoint), rights check (`R_SEND`).
7. Rendezvous: the kernel's waiting receive completes; kernel allocates the (kernel-internal) reply continuation; the echo responder checks `tag == TAG_PING` and replies with `MsgBuf { tag: TAG_PONG, data_len: 1, data[0]: u64::from_le_bytes(*b"PONG\n\0\0\0"), handle_count: 0, .. }` — exercising recv, reply, and the reply-capability path.
8. `sys_call` returns `0` in `rax`; the server's buffer now holds the reply. Server asserts `tag == TAG_PONG`.
9. Server invokes **`sys_console_write(BOOT_CONSOLE, msg.data.as_ptr() as *const u8, 5)`** — second independent handle lookup + rights check (`R_WRITE`). The serial console now shows the line `PONG`.
10. Server invokes **`sys_exit(0)`**. Kernel logs `oxbow: server exited (0)` and halts.

**Pass criterion:** QEMU serial output contains, in order: a kernel boot banner, the exact bytes `PONG\n` (emitted by step 9, not by kernel code), and the exit log line. The trace has exercised: user-mode entry, the `syscall` path, capability lookup with rights enforcement (twice, on two object types), IPC call/recv/reply rendezvous with payload copy, and ELF loading under W^X.

---

## 8. Explicitly deferred (not in v0)

- **Threads** beyond one-per-process; any scheduler beyond "run the one ready thing"; priorities; preemption tuning.
- **Real memory accountability:** Untyped/Frame capabilities, retyping, user-driven `sys_map`; v0 uses kernel static pools per L6's stated simplification.
- **Process/VSpace objects and `sys_spawn`/`sys_exec`:** the kernel hand-builds the only process; userland process creation is v1 (syscall numbers 8+ reserved).
- **Endpoint badges**, non-blocking/timeout IPC variants (`WouldBlock` is reserved), and notification (async signal) objects.
- **Shared memory / out-of-line message data** (messages are 8 words + 4 handles, period).
- **IRQ capabilities and device drivers** beyond the kernel-owned serial Console object.
- **Multicore** (kernel is single-core, synchronous, coarse-locked).
- **A POSIX shim / filesystem / naming server** — there is no name anywhere in this ABI, by law L3.

---

## 9. ABI additions — user-driven memory (v1 arc)

Appended, not a rewrite of v0. `ABI_VERSION` stays 0 (additions are append-only;
nothing in §1–8 changes). Syscall numbers 8–10, reserved by §4.2, are now assigned.
This partially discharges §8's "real memory accountability" bullet: the kernel no
longer allocates user frames from static pools — every frame is debited against a
**Memory** capability the caller holds (law L6 is now literally enforced).

### 9.1 New objects
- **Memory** — a byte *budget* (the degenerate seL4 "untyped"). Authorizes
  allocation; `sys_map`/`sys_frame_alloc` debit it; exhaustion → `E_NOMEM`.
  Rights: `R_MAP`, `R_GRANT`, `R_ATTENUATE`. A process is born holding one at
  `BOOT_MEM = 3` (256 KiB).
- **Frame** — one physical frame, nameable and mappable. Because handles transfer
  over IPC, a Frame can be *shared* between address spaces. Rights: `R_MAP` (may
  map), `R_WRITE` (may map *writable* — object-specific reuse of bit 16),
  `R_GRANT`, `R_ATTENUATE`. Attenuating away `R_WRITE` yields a read-only share.

`R_MAP = 1 << 17`. Mapping protection (per call, NOT rights): `PROT_READ = 1`,
`PROT_WRITE = 2` (implies read). **No exec** — W^X (L4) forbids writable+exec and
there is no `mprotect`, so an anonymous executable page would be useless.

### 9.2 Syscalls
- **8 `sys_map(mem, vaddr, len, prot)` → `SysResult`** — map `len/4096` anonymous,
  zeroed pages at `vaddr` in the caller's own address space, debiting `mem`
  (intermediate page tables are charged too). All validation precedes any side
  effect; the map cannot partially fail. Errors: `E_BAD_HANDLE/E_BAD_TYPE`,
  `E_RIGHTS` (no `R_MAP`), `E_MSG` (mis-aligned/zero len/bad prot), `E_NOMEM`
  (budget), `E_FAULT` (range wraps, leaves the lower half, or overlaps any present
  mapping).
- **9 `sys_frame_alloc(mem)` → `SysResult<Handle>`** — debit one frame from `mem`,
  mint a Frame, return its handle (`R_MAP|R_WRITE|R_GRANT|R_ATTENUATE`).
- **10 `sys_frame_map(frame, vaddr, prot)` → `SysResult`** — map that specific
  Frame in the caller's AS. `PROT_WRITE` requires `R_WRITE` on the *handle*
  (`E_RIGHTS` otherwise) — the read-only-share enforcement point.

### 9.3 Still deferred to a later "untyped/retype" arc
`sys_retype` (minting kernel objects from a Memory cap — kernel pools stay
static); ranged untypeds + a derivation tree + revocation; `sys_unmap`/`free` +
a free-capable PMM (this arc is map-only — growth is bounded by the budget); a
`VSpace` object (`sys_map` implicitly targets the caller's own AS); per-call
charging of `sys_frame_map`'s intermediate tables.

---

## 10. ABI additions — IRQ / device drivers (v1 arc)

Append-only (ABI_VERSION stays 0). Lets a USER process be a device driver:
hardware access becomes a capability. Syscall numbers 11–17.

### 10.1 New objects
- **Notification** — a counting, latching async semaphore. `signal` is callable
  from an IRQ handler (never blocks); `wait` blocks the caller until signalled
  and returns the latched count. A signal with no waiter latches (it is never
  lost). One waiter per notification. Rights: `R_SIGNAL` (= `R_SEND`), `R_WAIT`
  (= `R_RECV`), `R_GRANT`, `R_ATTENUATE`.
- **IoPort `{base, len}`** — authorizes `in`/`out` over a contiguous port range
  (8-bit this arc). Rights: `R_IN` (1<<18), `R_OUT` (1<<19), `R_GRANT`,
  `R_ATTENUATE`. Attenuating away `R_OUT` yields a read-only port handle.
- **IrqLine `{line}`** — authorizes binding/acking a hardware IRQ line. Rights:
  `R_BIND` (1<<20), `R_ACK` (1<<21), `R_GRANT`, `R_ATTENUATE`. The kernel mints
  hardware caps once, at boot, into the designated driver's table (L1 holds:
  authority lives in a handle, not a global; the kernel is the root resource).

### 10.2 Syscalls
- **11 `sys_notif_create()` → Notification handle.**
- **12 `sys_notif_signal(notif)`** — requires `R_SIGNAL`.
- **13 `sys_notif_wait(notif)` → count in rdx** — requires `R_WAIT`.
- **14 `sys_io_in(ioport, port)` → byte in rdx** — requires `R_IN`; `E_MSG` if
  the port is outside the cap's range.
- **15 `sys_io_out(ioport, port, value)`** — requires `R_OUT`.
- **16 `sys_irq_bind(irq, notif)`** — requires `R_BIND` on the line + `R_SIGNAL`
  on the notification; routes IRQ → notification. Does not unmask.
- **17 `sys_irq_ack(irq)`** — requires `R_ACK`; re-arms (unmasks) the line.

### 10.3 IRQ delivery discipline
On fire, the kernel handler MASKS the line, EOIs in the kernel (never deferred —
an in-service ISR bit held across a context switch freezes equal/lower lines),
and signals the bound notification (wake-only, no block). The driver waits,
drains the device, then `ack`s (unmask) — drain-before-ack is mandatory for an
edge-triggered line. IRQ0 stays kernel-internal (the scheduler tick); no `Irq(0)`
cap is ever minted, so the capability is the policy.

### 10.4 Deferred
LAPIC/IOAPIC/MSI; IRQ sharing; I/O widths > 8-bit; port-range derivation;
immediate dispatch on signal (driver latency is currently ≤1 tick); multi-waiter
notifications; untyped/retype for these pools.

---

## 11. TTY / shell protocol (v1-tty-shell)

The terminal is three userspace processes over one endpoint — no new syscalls.

### 11.1 Topology
`BOOT_TTY` (= `BOOT` handle 7) names the **TTY endpoint** (`EP1`). The kernel
hands it out at boot with role-split rights:
- **kbd** (module 2): `R_SEND` — posts keystrokes.
- **tty** (module 3): `R_RECV` — the *sole* receiver; owns the Console.
- **shell** (module 4): `R_SEND` — posts read-requests and output.

The tty is the only writer to the Console. The shell's default Console grant is
**revoked at boot** (`p.close(BOOT_CONSOLE)`), so it holds zero direct hardware
authority — all of its output flows through the tty. (L1 holds: authority is a
handle, not a global; least privilege is enforced by *not minting* the cap.)

### 11.2 Messages (one endpoint, tag-multiplexed)
A single blocking `recv` loop in the tty can't select across senders, so the
sender's intent rides in the message **tag**:
- **`TAG_TTY_CHAR`** (kbd/serial, one-way `send`): `data[0]` = one byte. Runs the
  line discipline: printable → buffer (echo live iff a reader waits, else defer —
  see §12.5); backspace (`0x08` or `0x7F`) → rub-out `\x08 \x08` if the char was
  on screen, else silent; CR/LF → terminate the line.
- **`TAG_TTY_READ`** (shell, `call`): "give me the next complete line." If one is
  queued, the tty replies immediately with `TAG_TTY_LINE`; otherwise it **stashes
  the Reply handle** and replies when the next line completes. Completed lines
  arriving with no waiter queue in a small FIFO (depth 4).
- **`TAG_TTY_WRITE`** (shell, one-way `send`): NUL-terminated payload; the tty
  writes it to the Console verbatim. Payloads > 63 B are chunked by the sender.
- **`TAG_TTY_LINE`** (tty → shell, the `READ` reply): NUL-terminated line bytes.

### 11.3 Shell builtins
`echo <text>` (prints the remainder of the line), `help` (lists builtins); an
empty line is a no-op; anything else → `oxbow: <cmd>: command not found`. The
prompt is `oxbow$ `, written via `TAG_TTY_WRITE` before each `READ`.

### 11.4 Echo ordering
Keystroke echo is synchronized with the reader so it never precedes the prompt or
tangles with shell output, even under paste / type-ahead — see §12.5 (cooked-mode
echo synchronization). The only residual edge is a paste of more than the
4-deep line FIFO while the shell is busy (excess lines dropped).

---

## 12. Serial console (v1-serial-console)

A userspace driver makes COM1 a real input device, so you can type at the shell
directly over the serial line (`just run`) — not just the PS/2 keyboard. It is
the §10 IRQ/driver pattern applied to the 16550 UART, with one twist: the device
is **shared with the kernel**, split by direction and enforced by capabilities.

### 12.1 Register ownership (kernel vs driver)
- **Kernel owns all config + the TX path.** `init()` (the `uart_16550` crate)
  programs LCR/divisor/MCR and leaves `IER=0x01` (RX-data interrupt enabled) and
  `MCR=0x0b` (OUT2 set — gates the IRQ onto the PIC). The kernel then retunes the
  FIFO to `FCR=0x07` (RX trigger level **1 byte**), so one keystroke raises IRQ4
  deterministically. Output stays polled THR writes under the `SERIAL1` lock.
  **The kernel must not re-init the UART after boot** (that would re-arm IER).
- **Driver owns the RX path, read-only.** It is granted `IoPort{0x3F8,1}` (RBR)
  and `IoPort{0x3FD,1}` (LSR) with **`R_IN` only — no `R_OUT`**. A driver write
  to any UART register is an `E_RIGHTS` fault: the capability *is* the ownership
  boundary. The driver writes **zero** UART registers; everything it needs is
  already configured by the kernel.

### 12.2 Boot handles (module 5, `servers/serial`)
- **`BOOT_SERIAL_IRQ = 4`** — `Irq(4)`, rights `R_BIND | R_ACK` (no GRANT/ATTEN).
- **`BOOT_SERIAL_RBR = 5`** — `IoPort{0x3F8,1}`, rights `R_IN`.
- **`BOOT_SERIAL_LSR = 6`** — `IoPort{0x3FD,1}`, rights `R_IN`.
- **`BOOT_TTY = 7`** — `Endpoint(EP1)`, rights `R_SEND` (forwards keystrokes).

(Handle slots are per-process, so reusing 4/5/6 here — as the kbd driver also
does — is not a collision; each process has its own table.)

### 12.3 Drain discipline
On IRQ4 the kernel handler masks line 4, EOIs, and signals the bound
notification. The driver then drains: `while LSR(0x3FD) bit0 (DR) set { read
RBR(0x3F8); forward as TAG_TTY_CHAR }`, then `sys_irq_ack` (unmask). With only
IER bit0 enabled, draining the RX FIFO below the trigger deasserts the
interrupt — **no IIR read is required**. drain-before-ack as in §10.3.

### 12.4 Line discipline note
The serial driver is a dumb byte pipe — no translation. Terminals send **DEL
(0x7F)** for Backspace (vs the PS/2 path's 0x08), so the tty line discipline
(§11.2) treats **both 0x08 and 0x7F** as backspace. Enter arrives as CR (0x0D),
already handled. The tty's existing echo is the sole echo source (QEMU's stdio
chardev is in raw mode; tcp has no echo) — no double echo.

### 12.5 Cooked-mode echo synchronization (v1-cooked-tty)
Echo is synchronized with the reader so paste / type-ahead never tangles with the
shell's output. The tty tracks an `echoed` cursor into the edit buffer
(`edit[..echoed]` is on screen, `edit[echoed..elen]` is pending-echo) and gates
echo on whether a reader is waiting (`pending != HANDLE_NULL`):
- **A reader is waiting (normal interactive):** printable keystrokes echo live, as
  typed — unchanged behavior.
- **The shell is busy** (running a command, emitting output + the next prompt):
  keystrokes buffer **un-echoed**. Backspace edits the buffer silently (a char
  that was never shown is removed without a rub-out). A line completed in this
  window queues in the done FIFO un-echoed.
- **On the next `READ`:** the shell's prompt `TAG_TTY_WRITE` has already printed
  (the kernel endpoint is FIFO-ordered, and the shell sends the prompt *before*
  the READ), so the tty now flushes the echo — the in-progress line's pending
  tail on a stash, or a queued completed line at the moment it is popped. Each
  command's echo therefore lands grouped with its own prompt.

Invariant: a line resting in the done FIFO across loop iterations is always
un-echoed (it completed with no reader), so it is echoed exactly once, at
delivery. Children writing via `TAG_TTY_WRITE` while the shell blocks elsewhere
(e.g. `run pong`) pass through untouched, and type-ahead during such a command
buffers and flushes cleanly when the shell returns to `READ`. The only residual
edge is a paste of more than `DONE_CAP` (4) complete lines while busy: excess
lines are dropped with `[tty] !line dropped`, as before.

---

## 13. Process spawning (v1-spawn)

The shell launches programs at runtime; the boot no longer runs the pong/beta
demo (it goes straight to the prompt). Demo/program binaries that are not
boot-spawned are **registered as Image capabilities** and launched on demand.

### 13.1 Image objects
A spawnable program is an `Image` object (a registered Limine-module blob). The
shell is born holding three Image handles — `BOOT_IMG_HELLO=8`, `BOOT_IMG_PONG=9`,
`BOOT_IMG_BETA=10` — each with `R_SPAWN | R_GRANT | R_ATTENUATE`. A process can
only launch images it holds a handle to (zero ambient authority: spawn-by-handle,
never spawn-by-name-string). `R_SPAWN = 1<<22`.

### 13.2 `sys_spawn(image, mem, &MsgBuf, exit_notif) -> pid` (18)
Loads `image` into a fresh address space and starts it. Validation order
(all before any side effect): `image` (Image, `R_SPAWN`) → `mem` (Memory,
`R_MAP`) → `exit_notif` (Notification, `R_SIGNAL`, or `HANDLE_NULL`) → MsgBuf
pointer (8-aligned, mapped) → per-grant `R_GRANT` → image ELF validation →
budget bound. `pid` (informational, no authority) is returned in rdx.

The spawn **MsgBuf** is kernel-interpreted:
- `data[0]` = the child's Memory budget in bytes (0 → `SPAWN_DEFAULT_BUDGET`,
  256 KiB).
- `handles[0..handle_count]` = capabilities to grant the child (each non-null
  needs `R_GRANT` in the parent, §3.4 semantics — rights copied as-is). They land
  in the child's table at `SPAWN_SLOTS = [1, 2, 4, 5]`, in order; a `HANDLE_NULL`
  entry skips its slot. **Slot 3 is always the child's fresh Memory budget**, so
  it is not in `SPAWN_SLOTS`. `SPAWN_STDOUT = 2` is the conventional output
  endpoint (a tty `R_SEND` handle) — it shares `BOOT_CONSOLE`'s number, so a
  program that printed via `BOOT_CONSOLE` needs no slot change, only a switch
  from `sys_console_write` to a `TAG_TTY_WRITE` send.

### 13.3 Memory: the parent pays, no refund
`sys_spawn` charges the parent's Memory budget for the child's load pages + 16
stack pages + a page-table overhead + the child's budget bytes. The kernel
*checks* the parent can afford it before any side effect (so a later slot-full
failure costs nothing), then debits after the child is built. The bytes are
**never refunded** on child exit — `pmm` is a bump allocator with no frame
reclamation, so the frames really are gone; only the child's Memory *pool slot*
is freed (for reuse). The shell, as the system spawner, is born with an 8 MiB
budget; everything else gets 256 KiB.

### 13.4 Lifecycle: exit notification + reaping
The parent passes a Notification as `exit_notif`; the kernel signals it (a
counting notification, once) when the child dies — on `sys_exit` or a ring-3
fault, which converge in `proc::kill`. `kill` abandons the child's Replies (so a
blocked caller wakes `E_GONE`), frees the child's Memory slot, marks the process
slot reusable, and signals the parent. A parent waits with `sys_notif_wait`,
summing the drained counts to the number of children spawned. There is no exit
*status* in v1 (deferred). Fire-and-forget = pass `HANDLE_NULL`.

Process and thread slots are reused on death (a `Dead` process / `Exited` thread
slot is reclaimed by the next spawn — safe because an exited thread never resumes
on a single CPU with IF=0). Spawn-when-full / over-budget is a clean `E_NOMEM`,
never a panic.

### 13.5 `sys_ep_create() -> Endpoint handle` (19)
Mints a fresh endpoint (`R_SEND|R_RECV|R_GRANT|R_ATTENUATE`) so a parent can wire
an IPC channel between the children it spawns (e.g. `run pong` gives beta the
attenuated recv side and pong the send side). No reclamation in v1 (the pool is
bounded; a long-lived shell mints only one).

### 13.6 Deferred
Exit *status* codes; argv/environment; frame reclamation (which would also let
budgets refund) + endpoint/notification pool free; passing Image handles to
non-shell holders (a real init/launcher); a Process handle for kill/wait by the
parent. The `--features selftest` Console-probe path on pong is orphaned by the
move off boot-spawn and needs reworking into a spawnable test.

---

## 14. Badged endpoints (v1-badged-ep)

The seL4 badge mechanism: a holder of an endpoint capability can mint additional
capabilities to the **same** endpoint, each stamped with a server-chosen
**badge**. When a message is sent through a badged cap, the kernel delivers the
badge — unforgeably — to the receiver. This lets one server multiplex many
unforgeable per-object/per-client capabilities on a **single** endpoint (no
per-object endpoint objects, no wait-on-many primitive). It is the foundation for
the filesystem: each open file will be a badged capability to the one FS endpoint.

### 14.1 The badge field
A badge is per-**capability** state (on the handle, like rights — law L2), not on
the endpoint object: many handles to one endpoint pool slot carry different
badges. `0` = unbadged (the default for every boot grant, `sys_ep_create`, and
`MsgBuf::new`).

`MsgBuf` gains a trailing `badge: u64` (struct is now 104 bytes, pinned by a
compile-time assert). On delivery the kernel writes the **invoked cap's** badge
into the receiver's MsgBuf, overwriting whatever the sender put there — so the
sender cannot forge a badge. An unbadged send delivers `0`. A **reply always
delivers `0`** (badges are forward-only: they identify the caller *to* the
server, and a reply is already directed).

### 14.2 `sys_mint(src, badge, new_rights) -> Handle` (20)
Derives a badged capability to the endpoint `src`. Validation order
(handle → type → rights → args, before side effects):
- `src` resolves (`E_BAD_HANDLE`), is an Endpoint (`E_BAD_TYPE`).
- `src` held with **`R_ATTENUATE`** (`E_RIGHTS`) — minting is a derivation, the
  family `R_ATTENUATE` gates. (`R_GRANT` is orthogonal: it means "may ride in a
  message", §3.4.)
- **`src.badge == 0`** (`E_RIGHTS` otherwise) — **re-badging is forbidden**. This
  immutability is the whole security property: a holder of a badge-7 cap cannot
  manufacture a badge-42 cap to the same endpoint.
- `new_rights ⊆ src.rights` (`E_RIGHTS`) — no amplification (law L5).
- `badge != 0` (`E_MSG`) — `0` stays unambiguously "unbadged".
- Returns a new handle: same `Endpoint(idx)`, `rights = new_rights`, the chosen
  badge. Full `u64` badge range.

### 14.3 Preservation
A badge is set ONCE by `sys_mint` and never changed afterward:
- **`sys_attenuate`** on a badged cap preserves the badge (drops rights only) —
  so a read-only file handle is a badged cap with fewer rights.
- **Message transfer** (§3.4) and **spawn grants** (§13) copy the whole
  `HandleEntry`, badge included — a client can hand a badged cap onward unforged.

### 14.4 Acceptance (the `badge` demo, `BOOT_IMG_BADGE = 11`)
`badgetest` from the shell mints badges 7 and 42 off its endpoint, spawns the
`badge` server (granted the recv side at slot 1), and sends one message through
each badged cap plus one through the unbadged endpoint with a sender-written
`badge = 1234`. The server reports the kernel-delivered badges:

```
[badge] got 7      (sender-first delivery)
[badge] got 42     (receiver-first delivery — both orderings in one run)
[badge] got 0      (forgery blocked: the kernel overwrote the sender's 1234)
[badge] done
```

Plus the mint negative paths: re-badge denied, badge-0 denied, amplify denied,
non-endpoint denied.

---

## 15. Filesystem (v1-ramfs)

A userspace in-memory filesystem server (`servers/fs`), reached entirely through
capabilities — there is NO kernel-resident namespace (laws L1/L3). Read-only in
this arc (`ls`/`cat`); write is a follow-on.

### 15.1 Directories are capabilities
You open a file *relative to a directory capability you already hold*. The shell
is born holding the **root directory capability** at `BOOT_FS_ROOT = 12`: a
BADGED endpoint to the fs server with `badge = FS_ROOT (1)` and `R_SEND|R_GRANT`.
There is no path you can name without first holding a directory cap. Holding a
dir cap = authority over that subtree and nothing above it (OPEN rejects `/`,
`..`, `.` — confinement).

### 15.2 Each open file/dir is a badged capability; the server is stateless
A badge carries the node id (§14, kernel-stamped, unforgeable), so every request
arrives identifying its node — the server just indexes `nodes[badge]`. There is
**no open-file table, no fids, no per-client state, no seek state**. OPEN mints a
fresh badged cap (`badge = resolved child node id`) via `sys_mint` and returns it
in the reply (reply-carried handle transfer — see below); the FS closes its own
copy after the transfer. CLOSE is just the client closing its handle.

### 15.3 Reply-carried capabilities (general IPC change)
`sys_reply` now transfers handles like the forward path (§3.4): each handle in a
reply needs `R_GRANT` in the replier's table, and the kernel installs it in the
caller's table (rewriting the index). This is what lets OPEN hand a freshly-minted
file cap back to the client. A reply still delivers `badge = 0` (§14); the
*transferred handle* carries its own badge (the node id).

### 15.4 Protocol (tag-multiplexed on the fs endpoint, dispatch on `m.badge`)
- **`TAG_FS_OPEN`** (call through a dir cap): request `data` = NUL-terminated
  name. Reply: `data[0]` = status (0 ok / 1 not-found), `data[1]` = kind
  (`FS_DIR`/`FS_FILE`), `data[2]` = size, `handles[0]` = minted node cap.
- **`TAG_FS_READ`** (call through a file cap): `data[0]` = byte offset. Reply:
  `data[0]` = count (0 = EOF), `data[1..]` = up to 56 bytes. Clients loop.
- **`TAG_FS_READDIR`** (call through a dir cap): `data[0]` = cursor. Reply:
  `data[0]` = 1 if an entry follows (else 0 = end), `data[1]` = kind,
  `data[2..]` = entry name. Clients loop.

### 15.5 The tree + the tar initrd
The fs holds a static node pool (`{kind, name, parent, content}`); ROOT = node 1.
A **USTAR tar** archive is delivered as a Limine module (`initrd`); the kernel
maps its frames **read-only (NX)** into the fs address space at `FS_INITRD =
0x1000_0000` at boot. The fs parses the USTAR headers (stopping at the zero
end-block) and creates a file node per top-level regular file, with content
pointing **directly into the mapped archive** (no copy — read-only ramfs). The
archive is built at compile time from `servers/fs/initrd/`.

### 15.6 Deferred
Write (`WRITE`/`CREATE`/`MKDIR`, file growth from the fs Memory budget),
subdirectory traversal / multi-component path walk, zero-copy frame transfer for
bulk read/write, spawned coreutils (receiving an open-file cap), `STAT`, refcount
on CLOSE, a real on-disk filesystem behind the same VFS interface.

### 15.7 Write, directories, current-directory (v1-fs-write)
The filesystem is now read-write. File bytes live in an **arena** the fs server
`sys_map`s from its *own Memory budget* (law L6 — even the filesystem funds its
storage from a capability it holds); the seed tar files are copied in at boot, so
every file is uniformly writable. Each file node is `{arena offset, len, cap}`
with a fixed per-file capacity (1 KiB in v1; growth/realloc deferred).

New protocol ops (tag-multiplexed on the fs endpoint, dispatch on `m.badge`):
- **`TAG_FS_CREATE`** (call through a dir cap): `data` = name. Create-or-truncate
  a file under the directory; reply `data[0]` = status, `handles[0]` = a badged
  cap to the file. `>` redirect uses this.
- **`TAG_FS_WRITE`** (call through a file cap): `data[0]` = offset, `data[1]` =
  count, `data[2..]` = up to 48 bytes. Reply `data[0]` = count written (0 = full).
  Clients loop for longer writes.
- **`TAG_FS_MKDIR`** (call through a dir cap): `data` = name → a child directory.

The shell gains `echo TEXT > FILE` (CREATE + WRITE loop), `mkdir`, and `cd`. `cd`
swaps the shell's **current-directory capability** (starts at root; `cd <name>`
opens a subdir cap, `cd /` returns to root). There is no `cd ..` — you cannot
walk above a directory capability you hold (confinement); `cd /` works only
because the shell still holds the root cap. Files created in a subdirectory are
not resolvable from a parent's cap, so the capability tree *is* the access-control
boundary.

§15.6's deferred list still stands minus write/mkdir/cd, which are now done; file
growth/realloc, multi-component path walk, zero-copy frame bulk transfer, spawned
coreutils, rm/unlink, and an on-disk FS remain deferred.

### 15.8 Spawned coreutils (v1-coreutils)
`ls` and `cat` are no longer shell builtins — they are spawnable programs
(`servers/{ls,cat}`, images `BOOT_IMG_LS`/`BOOT_IMG_CAT`) the shell launches via
`sys_spawn`. This is the first capability transfer between *unrelated* processes:

- **`cat <name>`**: the shell (which holds the directory cap) resolves the name
  via OPEN, gets a badged file capability, and grants it to a freshly-spawned
  `cat` at slot 1 (`BOOT_EP`); stdout (a tty endpoint) at slot 2. `cat` loops READ
  on slot 1 and writes to slot 2, then exits. It never sees a filename and holds
  exactly one file, read-only — no directory, no namespace.
- **`ls`**: the shell grants the current-directory capability at slot 1; `ls`
  loops READDIR and prints, then exits.

A spawned coreutil cannot take a *name* argument (there is no argv yet), so the
shell must pre-resolve names into capabilities — which is exactly why `cat`/`ls`
spawn cleanly (they operate on a cap) while `mkdir`/`cd`/`echo >` stay builtins
(they need a name or shell state). The least-privilege story is literal: a
spawned `cat` is handed precisely the authority it needs and nothing more, and
the kernel enforces it — `cat` has no handle to any other file. (Deferred: argv,
which would let coreutils resolve their own names.)

### 13.7 Spawn arguments (argv) (v1-argv)
A spawned program can be given a single string argument. The parent packs it
into the spawn MsgBuf's `data[1..]` (byte offset 8, NUL-terminated, ≤55 bytes —
`data[0]` is still the budget). The kernel maps one read-only page into the child
at `SPAWN_ARGV = 0x0F00_0000` and writes the string there (always mapped, empty
if none; the +1 page is charged to the parent's budget). The child reads it via
`rt::argv()`.

This is what lets a coreutil take a *name*: `mkdir`/`touch` are spawned programs
granted the current-directory capability at slot 1 and the new name as argv —
they issue MKDIR / CREATE relative to the dir cap. A name-creating command can't
be expressed by cap-passing alone (the thing doesn't exist yet to be handed as a
capability), which is exactly the niche argv fills.

Note the deliberate split: read commands (`cat`, `ls`) operate on a *capability*
the shell pre-resolves and hands over (most confined — `cat` holds exactly one
file); name-creating commands (`mkdir`, `touch`) take a *name* via argv plus the
directory capability. Both are least-privilege; argv is not a license to widen
authority, only to name a target within authority already granted.

(Deferred: multiple arguments / a real argv vector; argument parsing beyond a
single token.)

### 15.9 Remove and rename (v1-rm-mv)
The first destructive filesystem operations, both spawned coreutils granted the
current-directory capability + a name via argv (§13.7):

- **`TAG_FS_UNLINK`** (`rm <name>`, dir cap): removes a file, or an *empty*
  directory. Reply `data[0]` = status (0 ok / 1 not-found / 2 directory-not-empty).
  Non-empty directories are refused (no `-r`).
- **`TAG_FS_RENAME`** (`mv <old> <new>`, dir cap): `data` = old name NUL then new
  name NUL; renames a child within the directory if `new` is free. `mv` splits
  its single argv string into the two names. Reply `data[0]` = status.

Both operate only on the children of the directory capability they hold —
confinement applies to destructive ops too: `rm`/`mv` cannot touch anything
outside the directory the shell handed them, and cannot escape via `..`.

Limitation: `rm` frees the file's node slot but NOT its arena bytes (the storage
arena is a bump allocator with no free), so deleted-file storage leaks until a
future arena free-list / compaction arc — the same deferred-reclaim story as the
frame allocator and Memory budgets. Cross-directory `mv` (two dir caps) is also
deferred.

---

## 16. Memory reclamation (v1-reclaim)

Earlier arcs deferred all reclamation: the frame allocator, Memory budgets, and
the fs arena were one-way. That capped a session at ~20 spawned commands (each
permanently consumed ~400 KiB of the shell's budget) and let `rm`'d file storage
leak. This arc closes the loop at every layer.

### 16.1 Physical frames
`pmm` gains an **intrusive free list**: a freed frame stores the next-free
physical address in its own first 8 bytes, so `free_frame` is O(1) and needs no
side table. `alloc_frame` pops a reclaimed frame before extending the bump
pointer.

### 16.2 Address-space teardown
`vm::free_user_pml4` walks the LOWER half (user, PML4 entries 0..256) of a dead
process's tables and frees every leaf data frame plus the intermediate
page-table frames, then the PML4 — returning it all to `pmm`. The upper half
(the shared kernel image + HHDM) is never touched. **Shared frames** (Frame
objects / zero-copy shmem, identified via the Frame pool) are *skipped* so a
peer's teardown can't double-free a frame the other still maps.

Timing: an address space is freed **on slot reuse** (`proc::create` reclaiming a
`Dead` slot), never on the dying thread itself — at exit the dying process's
PML4 is still the live CR3. By reuse time the owner has long switched away, so
the free is safe. (A dead slot never reused before shutdown simply isn't freed.)

### 16.3 Budget refund
A spawn records the spawner's Memory budget + the cost it paid; on the child's
death `proc::kill` **credits that cost back** to the spawner. So a shell that
spawns and reaps commands forever never exhausts its budget — verified by 70
back-to-back spawn cycles with no exhaustion.

### 16.4 Filesystem arena
The fs storage arena gains a free list (uniform `FILE_CAP` regions): `rm` returns
a removed file's region, and `CREATE` reuses it before extending. Repeated
create/rm cycles no longer leak the arena.

### 16.5 Frame reclamation (v1-frame-refcount)
A Frame object (zero-copy shmem) is **mapping-refcounted**: `sys_frame_map`
increments its count, and address-space teardown decrements it; when the last
mapping is torn down the physical frame and the pool slot are freed. So a shared
frame outlives any single mapper but is reclaimed once nobody maps it — no leak.
(A Frame allocated but never mapped, an edge oxbow's code never hits, is the one
remaining slow path.)

### 16.6 Still deferred
Freeing a `Dead` slot's frames at exit rather than at reuse (a reaper); file
growth / realloc. Bounded, not the unbounded per-command leak arc 16 removed.

### 15.10 Multi-component paths (v1-paths)
The fs now resolves `/`-separated paths, not just single names. OPEN walks the
full path from the invoked directory; CREATE/MKDIR/UNLINK resolve the parent path
and operate on the last component; RENAME resolves the source and destination
parents independently, so `mv a/x b/y` moves a node ACROSS directories (re-parents
it). Resolution descends only — `.` and `..` are rejected, so a path can never
escape above the directory capability it was invoked through (confinement is
preserved). Empty components (leading/trailing/double `/`) are tolerated. The
shell gains `ls <path>` (opens the directory, hands its cap to a spawned `ls`);
`cat`/`cd`/`echo >`/`mkdir`/`touch`/`rm`/`mv` already pass the path through.

---

## 17. Userland runtime ("libc") (v1-libc)

`oxbow-rt` grew from raw syscall stubs into a small libc, so programs read like
ordinary Rust instead of hand-packing MsgBufs.

### 17.1 Heap (`alloc`)
A bump allocator backed by the program's Memory budget, registered as the global
allocator — so `extern crate alloc` works (`Vec`, `String`, `Box`, `format!`).
It maps pages **lazily** from `BOOT_MEM` on first allocation (a program that never
allocates pays nothing) and grows page-by-page up to a 4 MiB ceiling. `dealloc`
is a no-op: spawned programs are short-lived and the whole address space is
reclaimed on exit (§16), so a bump heap is exactly right.

### 17.2 stdout: `print!` / `println!`
A `Stdout` sink implements `core::fmt::Write` over the program's stdout endpoint
(`SPAWN_STDOUT`, chunked into TAG_TTY_WRITE messages), backing `print!`/`println!`
— so `println!("squares {:?}", v)` Just Works.

### 17.3 File API (`rt::fs`)
A thin client for the fs protocol (§15): `open(dir, path) -> Node {cap, kind,
size}`, `read_all(file) -> Vec<u8>`, `readdir(dir, cursor) -> Option<(name,
kind)>`. The coreutils are rewritten against it — `cat` is `read_all` +
`stdout_write`; `ls` is a `readdir` loop with `println!`.

### 17.4 Deferred
A buffered/line-disciplined stdin, real `errno`, a heap free-list (the bump heap
suffices for short-lived programs), `mmap`/`sbrk`-style growth controls.

### 15.11 File growth (v1-file-growth)
A file is no longer a single fixed region — it is a list of up to `MAX_BLOCKS`
(16) arena BLOCKs, so it grows past one block (up to 16 KiB) as it is written.
WRITE allocates blocks on demand (spanning block boundaries); READ serves one
block per call (the client loops); truncate (CREATE on an existing file) and
remove return all the file's blocks to the arena free list (uniform BLOCK
regions, so arc-16 reclamation still holds). The shell gains `>>` (append):
`echo X >> f` opens the file and writes at its current end (creating it if
absent), so a multi-block file can be built a line at a time. The arena grew to
128 KiB (TOTAL_BLOCKS = 128) for larger working sets.

### 17.5 argv vector + cp (v1-argv-vector)
`rt::args()` splits the spawn argument string into whitespace tokens — a real
argv vector (`for a in rt::args()`), so a program takes any number of arguments
without re-implementing splitting. The file API gains `fs::create(dir, path)` and
`fs::write_all(file, bytes)`. The first genuinely two-argument coreutil, `cp src
dst`, is built on them: `open` src, `read_all`, `create` dst, `write_all` — and
`mv` now reads its two names via `args()`. (The single-string spawn mechanism is
unchanged; `args()` is purely a userland convenience over it.)

## 18. PCI / MMIO capability (v1-pci)

The first step toward a network stack is **not** networking — it is a capability
mechanism for talking to a real device. oxbow lets a userland driver hold a
`PciDevice` capability scoped to exactly one PCI function, read/write its config
space, and map its MMIO BARs — with no ambient authority over the bus or over
physical memory it was not granted.

### 18.1 Enumeration (kernel)
At boot (`kmain_stage2`, after IPC init) the kernel does a legacy PCI
configuration-space scan via I/O ports `0xCF8` (CONFIG_ADDRESS) / `0xCFC`
(CONFIG_DATA): 256 buses × 32 devices × 8 functions. Each present function is
logged; the first **network-class** function (class `0x02`) is remembered as the
boot NIC. Under QEMU's `-device e1000` this is `8086:100e at 00:02.0`, BAR0
`0xfebc0000` (128 KiB). Enumeration is kernel-only because it pokes a global
ports pair — it is not itself a capability operation.

### 18.2 The `PciDevice` capability
A new object type. `ObjectRef::PciDevice(u32)` packs the bus/device/function as
`bus<<16 | dev<<8 | func` (its "BDF"). It is granted from the boot loop: typing
`net` at the shell hands the resident `net` server a `PciDevice` cap to the
enumerated NIC with rights `R_IN | R_OUT | R_MAP` (config read / config write /
BAR map). The cap names **one function** — mirroring the IoPort/Irq model, a
driver gets its device, never the bus.

### 18.3 Syscalls
| # | name | rights | effect |
|---|------|--------|--------|
| 21 | `SYS_PCI_READ(dev, offset)` | `R_IN` | returns the config-space `u32` at `offset` (in `rdx`) |
| 22 | `SYS_PCI_WRITE(dev, offset, value)` | `R_OUT` | writes a config-space `u32` (e.g. the command register, to enable the device) |
| 23 | `SYS_PCI_BAR_MAP(dev, bar, vaddr)` | `R_MAP` | maps BAR`bar`'s MMIO region into the caller's address space at `vaddr`, uncacheable |

`SYS_PCI_BAR_MAP` reads the BAR's base+size by the standard write-`0xFFFFFFFF`/
read-mask/restore probe, then maps each 4 KiB page via `vm::map_mmio_4k_in`
with `PRESENT | USER_ACCESSIBLE | WRITABLE | NO_EXECUTE | NO_CACHE`. No frame is
consumed — a BAR is a device physical address, not RAM — so this never touches
the frame allocator. The mapped size is capped (≤ 1 MiB) so a bad BAR can't map
the world. `NET_MMIO = 0x4000_0000` is the conventional vaddr a driver maps its
registers to.

### 18.4 Proof: the `net` driver
`servers/net` is a resident boot module that, holding only `BOOT_PCI`:
1. reads config offset `0x00` → confirms `8086:100e`;
2. writes the command register (offset `0x04`) with bits 1+2 → memory-space
   decode + bus mastering enabled;
3. `SYS_PCI_BAR_MAP(BOOT_PCI, 0, NET_MMIO)` → BAR0 mapped;
4. reads real e1000 registers through MMIO: `RAL`/`RAH` (`0x5400`/`0x5404`) →
   the MAC `52:54:00:12:34:56`, and `STATUS` (`0x0008`) → `0x80080783`
   (link-up). It prints `[net] ready` and parks on a notification.

This establishes the substrate; the following arcs build the e1000 TX/RX
descriptor rings + IRQ on top, then Ethernet/ARP/IPv4/UDP, then smoltcp for TCP
and a socket capability API — all as userland servers over this mechanism.

## 19. e1000 NIC driver — rings, DMA, IRQ (v1-e1000)

Building on the PCI/MMIO capability (§18), `net` becomes a real driver: it owns
DMA descriptor rings, brings the e1000 up, and receives packets through an
interrupt. The arc is proven end to end — the driver hand-builds one ARP request,
transmits it, and receives the QEMU SLIRP gateway's ARP reply *via the NIC's
interrupt* — so TX, RX, and IRQ all work over the capability model.

### 19.1 DMA memory: `SYS_DMA_ALLOC` (24)
`sys_dma_alloc(mem, vaddr) -> phys` allocates one frame from the caller's Memory
budget (R_MAP), maps it writable+cacheable at `vaddr`, and **returns its physical
address**. A bus-mastering driver must know physical addresses to program a
device's ring-base registers and per-descriptor buffer pointers — virtual
addresses are meaningless to the device's DMA engine. The frame is an ordinary
lower-half mapping, so §16 reclamation frees it on AS teardown like any other. No
new authority is exposed: with no IOMMU in v0, a driver holding a bus-master
device cap can already DMA anywhere; revealing the physical address of *its own*
frames adds nothing. (Lesson learned the hard way: a TX descriptor pointing at a
*virtual* buffer address transmits whatever physical memory happens to live
there — garbage the gateway silently drops.)

### 19.2 The NIC interrupt: `BOOT_NET_IRQ`
The kernel enumerates the NIC's interrupt line from PCI config space (offset
0x3C — QEMU/SeaBIOS routes the e1000 to legacy PIC IRQ 11) and grants `net` an
`Irq(line)` capability (`BOOT_NET_IRQ`, rights `R_BIND | R_ACK`) alongside its
PciDevice cap. The driver `sys_irq_bind`s it to a Notification and `sys_irq_ack`s
to arm the line. IRQ 11 is a *slave-PIC* line, so the kernel's `pic::unmask` now
also unmasks the master's cascade input (IRQ2) — the first slave-line driver in
the system. A dedicated IDT handler (vector 0x2B) follows the standard
mask-on-fire / EOI-in-kernel discipline; the driver reads the device ICR (which
deasserts the level-triggered INTx) before acking. PCI INTx is level-triggered,
which the mask-on-fire rule handles without change.

### 19.3 e1000 bring-up (driver)
`servers/net` now: enables bus-mastering, maps BAR0, pulses `CTRL.RST`, sets link
up (`CTRL.SLU`), clears the multicast table, and builds two rings in DMA memory —
RX (8 descriptors, 2048-byte buffers, `RCTL = EN|UPE|MPE|BAM|SECRC`) and TX
(`TCTL = EN|PSP|CT|COLD`, standard TIPG). RX head/tail bracket the descriptors
hardware may fill; TX hands a descriptor to the device by writing the buffer's
*physical* address + `EOP|IFCS|RS` and bumping `TDT`. Receive is interrupt-driven:
park on the notification, read ICR, drain every descriptor whose `STA.DD` is set,
recycle it (clear status, advance `RDT`), then ack. Descriptor memory is
cacheable with a `SeqCst` fence before each tail bump — sufficient on x86's
coherent PCI bus (QEMU).

### 19.4 Demonstrated
`[net] ARP reply: 10.0.2.2 is at 52:55:0a:00:02:02` — the gateway's MAC, learned
by transmitting a broadcast ARP request and receiving the unicast reply through
IRQ 11. Next arcs layer proper Ethernet/ARP/IPv4/UDP (from scratch) and then
smoltcp for TCP, plus a socket capability API — all userland over this driver.

## 20. Network stack — Ethernet / ARP / IPv4 / ICMP / UDP (v1-udp)

With the e1000 driver (§19) moving frames, `net` grows the protocol layers from
scratch — one small module per layer, all pure byte-shuffling over the NIC's
`tx` / `recv_blocking`. No new syscalls: the whole stack is userland over the
capabilities the driver already holds.

### 20.1 Layers
| module | layer | does |
|--------|-------|------|
| `eth`  | L2 | Ethernet II frame build/parse (the NIC pads + appends FCS) |
| `arp`  | L2/3 | ARP request/reply build/parse + a small direct-mapped IPv4→MAC cache |
| `ipv4` | L3 | IPv4 header build/parse + the RFC 1071 internet checksum (shared) |
| `icmp` | L3 | ICMP echo request/reply (ping), checksummed |
| `udp`  | L4 | UDP datagram build/parse with the full IPv4 pseudo-header checksum |
| `dns`  | app | minimal A-record query builder + first-answer parser (name compression aware) |

### 20.2 Driver shape
`Nic` owns the rings/buffers/IRQ notification and exposes two primitives:
`tx(frame)` (round-robins the TX ring, hands the device the buffer's physical
address) and `recv_blocking(out)` (drains the RX ring, parking on IRQ11 when
empty). Everything above is layered calls that build a `Vec<u8>` bottom-up and
hand it to `tx`, or parse a received slice top-down. `handle_background` makes
the stack a good citizen for *any* inbound frame: it caches the sender's ARP
binding, answers ARP requests for 10.0.2.15, and echo-replies pings aimed at us.

### 20.3 Demonstrated (against QEMU SLIRP)
The driver runs three real exchanges at boot, then serves the network forever:
- **ARP** — resolves the gateway: `10.0.2.2 is at 52:55:0a:00:02:02`.
- **UDP/DNS** — a genuine recursive lookup through SLIRP's forwarder (10.0.2.3:53):
  `example.com -> 172.66.147.243`, proving UDP + IPv4 + checksums + ARP end to end.
- **ICMP** — pings the gateway and prints the echo reply (`seq 1`).

Addressing is the SLIRP default (us 10.0.2.15, gateway .2, DNS .3); DHCP and a
proper socket *capability* API (a separate server handing out bound-port caps),
plus smoltcp for TCP, are the next arcs.

## 21. Socket capability API (v1-socket)

The network analogue of "directories are capabilities" (§15): **sockets are
capabilities**. A client that holds only a network *control* capability can bind
a UDP socket and send/receive datagrams — the net server mints it a fresh badged
endpoint per socket, and the badge makes the server near-stateless, exactly
mirroring the fs file-cap model. No client touches the NIC, the protocol stack,
or any port it was not given.

### 21.1 The endpoints
A fourth kernel endpoint, `EP3`, is the network endpoint. Two capabilities name it:
- **net server** holds it as `BOOT_EP`, UNBADGED, with `R_SEND|R_RECV|R_GRANT|
  R_ATTENUATE` — the root of network authority (serve requests, mint socket caps,
  grant them back). Installed whether or not a NIC was found, so a client fails
  cleanly instead of blocking forever.
- **shell** (and any future client) holds it as `BOOT_NET_EP`, BADGED with
  `NET_CTL`, `R_SEND|R_GRANT` — the control channel. This is the net analogue of
  the shell's `BOOT_FS_ROOT`.

### 21.2 The protocol (all via `sys_call`)
| tag | invoked on | request | reply |
|-----|-----------|---------|-------|
| `TAG_UDP_BIND` ("UBND") | NET_CTL cap | `data[0]`=port (0=ephemeral) | `data[0]`=status, `data[1]`=bound port, **`handles[0]`=fresh badged socket cap** |
| `TAG_UDP_SENDTO` ("USND") | socket cap | `data[0]`=dst IPv4 (BE u32), `data[1]`=dst port, `data[2]`=len, bytes@24 (≤40) | `data[0]`=status |
| `TAG_UDP_RECVFROM` ("URCV") | socket cap | — | `data[0]`=len, payload@8 (≤56) |

On `BIND` the server allocates a socket-table slot and `sys_mint`s a new badged
endpoint (badge = socket id = slot+1, rights `R_SEND|R_GRANT`), returns it in the
reply, and closes its own copy — the kernel transfers the cap to the client. On
`SENDTO`/`RECVFROM` the kernel stamps the socket id into `m.badge`, so the server
reads it to find the bound port; a forged badge is impossible (the kernel
overwrites the sender's value with the invoking cap's badge). The whole DNS
exchange in `dns example.com` rides this: the shell builds the query
(`rt::dns`), the socket carries the raw UDP payload (`rt::udp`), and net never
sees a hostname — just a datagram on a bound port.

### 21.3 Server shape
The net server stops auto-running demos; after NIC bring-up + a gateway ARP
("net ready") it serves `BOOT_EP` forever. NIC I/O happens *synchronously inside
request handling* — `SENDTO` ARP-resolves + transmits, `RECVFROM` blocks on the
NIC (servicing background ARP/ping) until a datagram for the bound port arrives.
This sidesteps oxbow's single-thread-per-process model (no select() over the
endpoint and the IRQ at once); the RX ring buffers packets that arrive between
requests. `recv_blocking` now re-arms IRQ11 *before* parking, since the server
may drain the ring without ever waiting.

### 21.4 Limits / next
Payloads are inline in the 64-byte MsgBuf (send ≤40, recv ≤56) — enough for DNS,
but a shared-frame socket buffer is needed for bigger datagrams. The server is
single-client-at-a-time (a blocked `RECVFROM` holds the reply). DHCP (to lease
10.0.2.15 rather than assert it) and smoltcp-backed TCP sockets over this same
capability shape are the next arcs.

## 22. DHCP — leasing an address (v1-dhcp)

The net server no longer *asserts* 10.0.2.15; it **leases** an address from the
SLIRP DHCP server at boot via the standard DORA handshake, then uses the leased
IP as the source for every socket.

### 22.1 Why it lives inside net (not the socket API)
A DHCP message is the 236-byte BOOTP header + options (~300 bytes) — far past the
64-byte inline socket payload (§21). So DHCP runs *inside* the net server over
its own stack (`eth`/`ipv4`/`udp` + a new `dhcp` module), exactly as the internal
ARP/DNS demos did, using full 2 KiB NIC buffers. It is the server's own business
(acquiring its identity), not a client capability.

### 22.2 The handshake
`dhcp_acquire` runs DISCOVER → OFFER → REQUEST → ACK:
- **DISCOVER** (broadcast, UDP 68→67, IP 0.0.0.0→255.255.255.255, BOOTP broadcast
  flag set so the reply is broadcast back — we have no IP to unicast to yet).
- **OFFER** parsed for `yiaddr` + the server identifier (option 54).
- **REQUEST** echoes the offered IP (option 50) + server id.
- **ACK** confirms the lease; net adopts `yiaddr` as `Nic.our_ip` and reads the
  router (option 3), DNS (option 6), and subnet mask (option 1) from it.

`Nic.our_ip` (previously the `OUR_IP` constant) is `0.0.0.0` until the lease, then
the leased address; ARP replies, ICMP echo, and every `SENDTO` source-address use
it. If DHCP doesn't answer within a bounded number of frames, net falls back to
the well-known SLIRP lease (10.0.2.15) so boot never wedges.

### 22.3 Demonstrated
`[net] DHCP lease: IP 10.0.2.15  gw 10.0.2.2  dns 10.0.2.3` at boot, after which
`dns example.com` resolves with the leased IP as the UDP source. Lease renewal
(T1/T2 timers) and honoring the lease time are deferred — the SLIRP lease is
effectively permanent for a VM session. smoltcp-backed TCP sockets over the §21
capability shape remain the next arc.

## 23. TCP via smoltcp (v1-tcp)

The last piece of "from scratch through UDP, smoltcp for TCP": real TCP sockets,
exposed through the same capability shape as UDP (§21). smoltcp is the TCP state
machine; oxbow supplies the layer below (the e1000 as a smoltcp `phy::Device`)
and the layer above (the socket-capability glue + a clock).

### 23.1 The clock: `SYS_UPTIME_MS` (25)
TCP needs timers (retransmit, delayed ACK, TIME_WAIT). `sys_uptime_ms()` returns
the kernel's monotonic tick (100 Hz) in milliseconds. It is ambient and
unprivileged — a clock is not a capability — and feeds smoltcp's `Instant`.

### 23.2 e1000 as a smoltcp Device
`tcp::PhyDevice` implements `phy::Device` over the NIC (held by raw pointer:
`receive` must return an Rx+Tx token pair that would otherwise need two `&mut`
borrows; single-threaded use makes the pointer sound). Crucially the tokens hold
**fixed stack buffers, never heap** — the poll loops call `receive` thousands of
times and oxbow's bump allocator never frees (§17), so a per-poll `Vec` would
exhaust the budget in milliseconds (it did, the first time). The empty-ring path
allocates nothing.

### 23.3 No select(): busy-poll with a deadline
A TCP op drives `Interface::poll` in a loop until the socket reaches the wanted
state or an uptime deadline passes. DMA fills the RX ring independent of the IRQ,
so polling needs no interrupt — which sidesteps oxbow's one-thread-per-process
inability to wait on the endpoint and the NIC at once. smoltcp does its own ARP
and routing (default route = the DHCP gateway), so it resolves peers itself.

### 23.4 The protocol (same shape as UDP §21)
| tag | invoked on | request | reply |
|-----|-----------|---------|-------|
| `TAG_TCP_CONNECT` | NET_CTL cap | `data[0]`=dst IPv4 (BE u32), `data[1]`=port | `data[0]`=status, **`handles[0]`=badged TCP-socket cap** |
| `TAG_TCP_SEND` | socket cap | `data[0]`=len, bytes@8 (≤48) | `data[0]`=status |
| `TAG_TCP_RECV` | socket cap | — | `data[0]`=len (0=closed), bytes@8 (≤56) |
| `TAG_TCP_CLOSE` | socket cap | — | `data[0]`=status |

`CONNECT` blocks server-side through the three-way handshake, then mints a badged
socket cap (badge = socket id) the same way UDP `BIND` and fs `OPEN` do. The
socket table slot is now a `Sock::Udp(port)` or `Sock::Tcp(SocketHandle)`.

### 23.5 Demonstrated
`http <ip>` (shell): connect to `<ip>:80`, send `GET / HTTP/1.0`, print the
response. In QEMU, `http 1.1.1.1` reaches Cloudflare through SLIRP NAT and prints
a real `HTTP/1.1` response. **On real hardware** (a Proxmox KVM VM, e1000 on a
LAN bridge), the same build leases a real DHCP address and `http` reaches a LAN
host, the ASUS router's web UI, and the public internet (1.1.1.1) through the
router's NAT — all over the busy-polled smoltcp path. Per-connection socket buffers still leak into the
bump heap (no free), so a real `dealloc`/slab is the natural follow-up; a
shared-frame socket buffer would also lift the 48/56-byte inline payload cap.
