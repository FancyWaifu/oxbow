# OpenBSD Security Engineering — Reference Notes for oxbow

> Audience: oxbow (from-scratch capability microkernel in Rust, x86_64, seL4-leaned
> synchronous-rendezvous IPC, **zero ambient authority**, capabilities-as-handles,
> badged endpoints, W^X enforced, per-process ASLR slide, near-complete Rust std
> port, `SYS_PLEDGE` already present).
>
> Goal of this doc: map each OpenBSD mechanism to (1) what it is, (2) the threat it
> stops, (3) applicability to oxbow — explicitly flagging **REDUNDANT** (already
> bought by Rust safety + capabilities) vs **STILL VALUABLE**, and (4) a priority.
>
> Core framing repeated throughout: **OpenBSD is a C monolith with ambient
> authority. Most of its arsenal is compensation for those two facts.** oxbow is
> neither, so the interesting question is always "does this defend against a class
> oxbow's foundation already eliminates, or against a class that survives the
> foundation (unsafe Rust, the C/libc/tcc userland, RNG quality, logic bugs)?"

---

## Top takeaways for oxbow (exec summary)

1. **Most OpenBSD memory-corruption mitigations are REDUNDANT for the oxbow
   kernel and Rust userland — they re-create, at runtime and in hardware, the
   invariants Rust gives you at compile time.** RETGUARD, stack protector/SSP,
   strict-junk malloc canaries, MAP_STACK enforcement, `strlcpy`/bounds-checker
   culture: these all fight C's lack of memory safety. Spend ZERO effort
   reproducing them in safe Rust. Their residual value in oxbow is confined to
   (a) `unsafe` blocks and (b) the C world oxbow hosts (oxbow-libc programs, tcc
   output). Apply them THERE, not globally.

2. **The OpenBSD ideas that are NOT about memory safety are the ones worth
   stealing — they're orthogonal to Rust and to capabilities.** Highest value:
   **randomness quality** (a real ChaCha20 CSPRNG + `getentropy` + reseeding +
   randomized allocation/layout), **W^X/immutability made un-revocable**
   (`mimmutable`-style permanent mappings), and **"syscall origin" pinning**
   adapted to the capability-invocation path. None of these is obviated by Rust
   or by capabilities.

3. **pledge/unveil are a partial, retrofit re-invention of what oxbow already
   IS.** pledge = coarse ambient-authority reduction bolted onto a process that
   started with full authority; unveil = re-introducing a filesystem namespace
   restriction onto a global namespace OpenBSD couldn't remove. In oxbow a
   process only ever holds the caps it was handed — there is no ambient authority
   to drop and no global namespace to unveil. **Keep `SYS_PLEDGE` only as an
   ergonomic, defense-in-depth syscall-class filter for the hosted POSIX/libc
   programs that think in syscalls; do not let it become load-bearing for native
   capability code.** The capability model is strictly stronger.

4. **KARL (relink-the-kernel-every-boot) targets a threat oxbow's architecture
   already blunts, and is expensive to copy.** KARL exists because a monolithic
   kernel is one giant shared attack surface with stable internal offsets;
   randomizing them raises the bar against info-leak-then-ROP. oxbow's small
   microkernel + driverless-kernel design already shrinks that surface enormously,
   and Rust removes most of the leak/overwrite primitives. Per-boot kernel ASLR
   (slide the whole image) is cheap and worth doing; full per-boot RELINKING is
   high-cost, low-marginal-return — LOW priority.

5. **The single most transferable thing from OpenBSD is not a mechanism — it's
   the posture: secure-by-default, minimal attack surface, proactive audit,
   "apply the mitigation to the maximum extent possible."** oxbow's
   capability/zero-ambient design IS this posture expressed architecturally.
   Where OpenBSD reaches for a mitigation, oxbow should first ask "can the
   capability model make this threat unrepresentable?" and only add a mitigation
   when the answer is no (RNG, unsafe blocks, hosted C). Defense-in-depth still
   matters: a memory-safe capability kernel with one exploitable `unsafe` bug and
   no secondary mitigations is still ownable.

---

## How to read the priority tags

- **HIGH** — adopt; defends a class that survives Rust + capabilities.
- **MED** — adopt narrowly (e.g. only for the hosted C/libc world, or only as
  cheap defense-in-depth for `unsafe`).
- **LOW** — largely redundant given oxbow's foundation; copy only if cheap or if
  a specific subsystem reintroduces the threat.

---

## 1. pledge(2) and unveil(2)

### 1.1 pledge(2)
**What it is.** A process voluntarily restricts itself to named subsets of
syscall functionality ("promises": `stdio`, `rpath`, `wpath`, `inet`, `dns`,
`proc`, `exec`, …). Called at runtime after initialization. Violating the pledge
does **not** return an error — the kernel kills the process with `SIGABRT`. A
process can only ever *narrow* its pledge, never widen it. `execpromises` lets a
parent constrain what an `execve`'d child may pledge.

**Adoption model (incremental).** Start permissive, run the program, see what it
needs, tighten. The canonical pattern: do all your privileged setup (open files,
bind sockets, load TLS certs into memory), THEN `pledge()` down to the minimal
set before touching untrusted input. Programs like `nc` re-pledge per operation
("a different pledge for each blade"). Easy to learn, no subtle behavior change,
crash-on-violation makes missing promises obvious during testing.

**Threat it stops.** A compromised process (post-exploit) trying to do something
outside its job — open a raw socket, exec a shell, read arbitrary files. It
shrinks the *ambient* syscall authority a process carries by default.

**Applicability to oxbow.** This is the crux. pledge is OpenBSD **retrofitting
capability-like least-authority onto a process that the OS handed full ambient
authority at fork/exec**. oxbow processes start with *no* authority and acquire
only handed-in capabilities — so the pledge model is **largely REDUNDANT for
native oxbow code**: there is no ambient syscall authority to drop. The residual
value:
- oxbow already ships `SYS_PLEDGE`. Keep it as a **syscall-class filter for the
  hosted POSIX/libc/tcc programs** that are written in the syscall idiom and
  don't natively reason in capabilities. For them, a pledge-style "this libc
  program may only make `stdio`+`inet` class calls" is real defense-in-depth.
- Use it as a **belt-and-suspenders backstop on the capability model**: even a
  process holding a capability it shouldn't invoke can be statically barred from
  the syscall class. Cheap, additive.
- Do **not** let pledge become the primary access-control story for native code.
  A capability is unforgeable and fine-grained (this *one* endpoint); a pledge is
  coarse (a whole syscall class) and is an ambient-authority patch. The cap model
  is strictly stronger and finer.

**Priority: MED** — keep `SYS_PLEDGE` as ergonomic defense-in-depth for hosted C;
redundant as primary control for native capability code.

### 1.2 unveil(2)
**What it is.** `unveil(path, perms)` makes the filesystem *invisible* except the
paths you explicitly reveal, with per-path `r`/`w`/`x`/`c` permissions.
Un-unveiled paths return `ENOENT` (they don't appear to exist); unveiled paths
accessed beyond their perms return `EACCES`. Unveiling a directory covers
everything beneath it. After the first `unveil` the view only shrinks; a final
`unveil(NULL, NULL)` locks it. Implemented deep in the kernel's name-lookup path.

**Threat it stops.** A compromised process (that pledge alone can't contain
because it legitimately needs filesystem access — e.g. Chrome's renderer) walking
the *global filesystem namespace* to read your SSH keys or write outside its
working set. pledge takes away syscalls; unveil takes away the namespace.

**Applicability to oxbow.** unveil is **OpenBSD reconstructing a per-process
filesystem namespace because UNIX has a single global one it cannot remove.**
oxbow has **no global namespace** — a process reaches the ext2 fs server only
through a directory/file capability it was handed (your memory notes:
namespace-confinement Option A, shell opens `/bin` once and hands a dir cap; home
confinement; "no cap = the access control, no perm bits"). **This is unveil's end
state, achieved structurally and more strongly: an oxbow process cannot even name
a path it has no capability for — there is nothing to make "return ENOENT," it
simply has no handle.** So unveil is **REDUNDANT for oxbow** as a mechanism.

What to *carry over* is the **design discipline**, not the syscall: when you mint
a directory capability for a process, mint the *narrowest* sub-tree and the
*minimum* rights (read-only where possible), the way good unveil usage reveals
the minimum. That's a policy guideline for your shell/session/spawn machinery,
not a new kernel feature.

**Priority: LOW** (mechanism is redundant) — but the *minimization discipline*
when handing out dir caps is **HIGH** as a policy.

> **Key mapping.** pledge+unveil together = "give a full-authority UNIX process
> least authority over syscalls and the namespace, after the fact." oxbow gives
> least authority *a priori* and *unforgeably*. The capability model is the thing
> pledge+unveil are groping toward. Don't re-import them as load-bearing; keep
> only the syscall-class filter for hosted C.

---

## 2. Memory-corruption mitigations

This whole family exists to compensate for C's lack of memory safety. **For safe
Rust (the kernel and the std/Rust userland) the entire category is REDUNDANT** —
the bugs these stop (buffer overflow, UAF, type confusion, uninitialized reads)
are absent by construction. Their oxbow-relevant residue is narrow and worth
stating precisely, per mechanism.

### 2.1 W^X (write-xor-execute)
**What it is.** No page is ever both writable and executable. OpenBSD was first
to enforce it on every platform that supports it (since 2002). Combined with
`mimmutable` so permissions can't be flipped back.

**Threat it stops.** Classic code injection: write shellcode into a buffer, jump
to it. Forces attackers up to ROP/reuse.

**Applicability to oxbow.** oxbow **already enforces W^X.** This one is NOT
redundant in the Rust sense — W^X defends the *machine*, not the language, and is
exactly as relevant to a Rust kernel as a C one (Rust doesn't stop a logic/unsafe
bug from flipping page perms if you let it). The forward-looking lesson from
OpenBSD's csw2023 work: **make W^X transitions one-way / un-revocable where
possible** (see mimmutable, §2.6). The dangerous case for oxbow is the JIT/exec
path: tcc's `-run` and any on-device codegen need W then X. Mirror OpenBSD's
discipline: a region is W during emit, then flipped to X, and **never both**; the
flip should be a deliberate, capability-gated operation.

**Priority: HIGH** (already done; keep it absolute, especially across the
tcc/JIT W→X transition).

### 2.2 Strict ASLR (+ PIE, stack gap, library/relink randomization)
**What it is.** Randomize base of stack, heap (`mmap`), shared libs, and PIE
program text. OpenBSD adds a random stack *gap* (26 bits of address-space
placement + a 12-bit within-page gap), random `mmap` placement, per-boot relink
of libc and sshd so even intra-library offsets move.

**Threat it stops.** Predicting where code/data live, a prerequisite for ROP and
data-only attacks. Doesn't stop the bug; raises exploitation cost, defeated by
info leaks.

**Applicability to oxbow.** oxbow already has a **per-process ASLR slide.** This
defends the machine layout regardless of language, so it's **not made redundant
by Rust** — but note its *value* is lower in oxbow because the primitives that
turn a leak into an exploit (corruptible return addresses, forgeable pointers)
are mostly absent in safe code. Treat ASLR as cheap defense-in-depth for the
`unsafe` and C-hosted cases. Worth copying from OpenBSD:
- **Randomize the stack gap** (cheap, frustrates relative-offset attacks).
- **`MAP_CONCEAL`-style randomized placement of sensitive mappings.**
- Per-boot/per-exec relink of libc-equivalent is **high cost, low marginal value**
  for oxbow (Rust removes the leak→overwrite chain) — skip.

**Priority: MED** — keep the slide, add a randomized stack gap; skip per-boot
relinking.

### 2.3 RETGUARD (per-function return-address protection)
**What it is.** Compiler (clang) emits a per-function prologue/epilogue that
XORs the return address on the stack with a per-function cookie derived from the
stack pointer, and verifies it before `ret`. A 3rd-generation ROP mitigation
(Todd Mortimer). Also doubles as gadget reduction — it perturbs the byte stream
so fewer useful gadgets exist. Used in userland **and the kernel**. Related:
`-fret-clean` (scrub the return value off the stack to reduce info leaks),
`sigreturn`/`setjmp`/`longjmp` cookies, IBT/BTI (`endbr64`) enforcement.

**Threat it stops.** ROP / return-address overwrite — corrupting a saved return
address to chain gadgets.

**Applicability to oxbow.** **REDUNDANT for safe Rust.** A saved return address
cannot be corrupted by safe Rust code — there is no out-of-bounds stack write to
overwrite it. RETGUARD is pure C-monolith compensation. Residual value:
- **`unsafe` blocks** can in principle corrupt the stack — but RETGUARD is a
  clang feature; the Rust equivalent is the **stack protector** (`-Z
  stack-protector`, now available) which you could enable on the kernel/std as
  cheap insurance for `unsafe`. Marginal.
- The **hosted C world matters**: oxbow-libc programs and **tcc output** are
  classic C, fully exposed to ROP. If you want RETGUARD-class protection there,
  it has to come from the C compiler. tcc does **not** implement RETGUARD or
  stack cookies — so binaries oxbow compiles on-device are *less* hardened than
  OpenBSD's clang output. Flag this as a known gap, not a kernel task.
- **IBT/`endbr64`** is a hardware CFI feature orthogonal to language — enabling
  Intel CET/IBT on oxbow's exec path is reasonable cheap hardening for both Rust
  and C code.

**Priority: LOW for the Rust kernel** (redundant); **MED** as a note that
on-device-compiled C is an un-hardened soft spot — consider enabling CET/IBT
system-wide as the language-agnostic equivalent.

### 2.4 OpenBSD malloc (guard pages, junking, randomized allocation, chunk canaries)
**What it is.** A security-first allocator: random placement of allocations,
**guard pages**, **junk-fill** on alloc and free (catches use-of-uninitialized
and use-after-free), **chunk canaries** to detect linear overflows, randomized
chunk order, **unmap-on-free** of large allocations (so UAF faults instead of
silently reusing), limited chunk reuse, and allocator metadata protected via
`mprotect`/`mimmutable`. Tunable via `malloc.conf` (the "S" / strict-junk mode).

**Threat it stops.** Heap overflow, use-after-free, double-free, uninitialized
heap reads — the heap-side memory-safety bug classes.

**Applicability to oxbow.** **REDUNDANT for the Rust allocator world.** Rust's
ownership/borrow model eliminates UAF and double-free for safe code; the Rust
allocator doesn't need junk-fill or canaries because the bugs they detect can't
arise in safe code. BUT — two concrete oxbow surfaces keep this alive:
- Your memory notes describe oxbow-rt's **`hosted` feature exporting
  `__oxbow_alloc`** and a no_std libc owning alloc for C programs. **The C/libc
  heap that oxbow hands to tcc-compiled and oxbow-libc programs is exactly the
  threat surface OpenBSD malloc defends.** Adopting OpenBSD-malloc tactics in the
  **oxbow-libc allocator** (guard pages on large allocs, junk-on-free,
  unmap-on-free, canaries) is **genuinely valuable** — it's the one place oxbow
  runs unsafe-by-construction code at scale.
- The **`unsafe`/raw allocation paths inside the kernel** (DMA buffers for the
  e1000/virtio drivers, page allocators) could benefit from **guard pages and
  poison-on-free** as cheap defense-in-depth — these are the spots where a Rust
  bug *could* be memory-unsafe.

**Priority: MED** — adopt OpenBSD-malloc tactics specifically in **oxbow-libc's
allocator** (highest leverage) and optionally as guard-page/poisoning policy for
kernel DMA/page allocators. Redundant for the safe-Rust heap.

### 2.5 Stack protector / SSP (ProPolice) everywhere
**What it is.** Compiler inserts a stack canary between locals and the saved
return address; checked on function return. OpenBSD builds the *entire* base
system with it ("SSP everywhere"). The 1st-generation stack-smashing mitigation.

**Threat it stops.** Linear stack buffer overflow overwriting the return address.

**Applicability to oxbow.** **REDUNDANT for safe Rust** (no OOB stack writes).
Residual value identical to RETGUARD: enable Rust's `-Z stack-protector` on
kernel/std as near-free insurance against `unsafe` bugs, and accept that tcc
output is unprotected (tcc has no SSP). The OpenBSD *culture* point — "build
*everything* with the mitigation, no opt-outs" — is the transferable bit: if you
do enable a hardening flag, apply it uniformly, don't leave per-crate gaps.

**Priority: LOW** (redundant) / cheap to enable for `unsafe` insurance.

### 2.6 mimmutable(2) — immutable mappings
**What it is.** `mimmutable()` makes a region's permissions **permanent**:
subsequent `mmap`/`mprotect`/`munmap` on it fail with `EPERM`. OpenBSD applies it
automatically to ELF segments — `.text` is execute+immutable, `.rodata` is
read+immutable, `.data`/`.bss` read-write+immutable region, the stack's
read/write is immutable. So even after an attacker gains a write/exec primitive,
they **cannot flip `.text` to writable or make the stack executable**.

**Threat it stops.** Post-exploitation permission tampering — the step where an
attacker with a memory-write primitive turns off W^X, makes the stack executable,
or remaps code as writable. Closes the "W^X but I can call mprotect" loophole.

**Applicability to oxbow.** **This is one of the genuinely transferable ideas**,
and it's language-agnostic — Rust doesn't stop a logic bug or an `unsafe` path
from calling a "change this mapping's perms" operation. oxbow should ensure that
**once a mapping is set (text=X, rodata=R, the W^X state of a region), that
decision is one-way and capability-gated.** Concretely:
- The VM/mapping syscalls should support an **immutable/seal flag**; a process
  (or the loader) seals its `.text`/`.rodata` after load.
- The **JIT/tcc exec path** is the deliberate exception: it needs a *narrow,
  explicitly-capability-controlled* ability to allocate W-then-X memory. That
  ability should itself be an unforgeable capability (a "may create executable
  mappings" cap), not ambient — which is *more* than OpenBSD can express, and a
  natural fit for oxbow.

**Priority: HIGH** — immutable/sealed mappings are cheap, language-independent,
and turn oxbow's existing W^X from "policy" into "un-revokable invariant." The
capability angle (executable-mapping creation as a mintable cap) is a strict
upgrade over OpenBSD.

### 2.7 MAP_STACK enforcement
**What it is.** Stack memory must be mapped with `MAP_STACK`; the kernel checks
on syscall entry and on page fault that the stack pointer points into a
`MAP_STACK` region, and kills the process otherwise. Stops attackers from
"pivoting" the stack pointer into attacker-controlled heap/data (a common ROP
setup step).

**Threat it stops.** Stack pivoting — pointing `%rsp` at a fake stack in
heap/data to drive a ROP chain.

**Applicability to oxbow.** **REDUNDANT for safe Rust** (no primitive to corrupt
`%rsp`). It's a C-monolith ROP-defense. Marginal residue: if oxbow's hosted C
programs are a concern, MAP_STACK-style validation could be applied to them, but
the leverage is low and the implementation cost (syscall/fault-path checks)
nontrivial. Skip unless ROP against hosted C becomes a demonstrated threat.

**Priority: LOW.**

### 2.8 API/build culture: strlcpy/strlcat, linker warnings, static bounds checker
**What it is.** Safer string primitives, linker warnings on `strcpy`/`sprintf`,
a static bounds checker, `issetugid`, removal of unsafe functions tree-wide.

**Applicability to oxbow.** **REDUNDANT** — this is "make C less footgun-y."
Rust's `&str`/`String`/slices already provide bounds-checked, length-aware string
handling. The only place it lands is, again, **oxbow-libc**: provide
`strlcpy`/`strlcat` (not `strcpy`) so the C programs oxbow hosts inherit the safer
primitives. The *meta-lesson* — make the safe path the default path — is already
how Rust APIs are designed.

**Priority: LOW** (provide strlcpy/strlcat in oxbow-libc; otherwise N/A).

---

## 3. Kernel-specific mechanisms

### 3.1 KARL — Kernel Address Randomized Link
**What it is.** On every install/upgrade/**boot**, OpenBSD **relinks the kernel**
from its object files in a randomized order, producing a unique kernel image with
unique internal function/data offsets. Distinct from kernel ASLR (which slides a
fixed image): KARL changes the *internal* layout, so leaking one address doesn't
reveal the rest. A boot runs the freshly-linked kernel and links the *next* one
in the background (<1s on fast machines).

**Threat it stops.** An attacker who obtains a kernel info leak and wants stable
internal offsets to build a kernel ROP chain / locate a target function. Each
reboot invalidates a leak-derived exploit.

**Applicability to oxbow.** KARL is **compensation for a large monolithic kernel
with a huge, stable, shared attack surface.** oxbow's design already attacks that
problem at the root: a **small microkernel** with drivers/net/fs pushed to
userspace means there's far less kernel code to leak/target, and Rust removes
most leak/overwrite primitives. So KARL's *marginal* value for oxbow is low, and
its *cost* is high (you'd need a boot-time relink toolchain in the boot path).
What's worth doing:
- **Per-boot kernel image ASLR (slide the whole image at boot)** — cheap, you
  likely have the slide machinery already; do this.
- **Full per-boot relinking** — HIGH cost, LOW marginal return given the small
  surface + Rust. Skip unless oxbow's kernel grows monolithic (it shouldn't, by
  design).

**Priority: LOW** (full KARL) — but **per-boot whole-image kernel ASLR is MED and
cheap.** The architectural takeaway: oxbow *prevents* the threat KARL *randomizes
around* by keeping the kernel small.

### 3.2 pinsyscalls(2) / msyscall — syscall-origin enforcement
**What it is.** The kernel records, via the `.openbsd.syscalls` ELF section, the
**exact address of every syscall instruction** in `libc.so` (and ld.so / static
binaries). `ld.so` calls `pinsyscalls(2)` to register them. The kernel then
**rejects any syscall not issued from its registered, per-syscall-number
location** — wrong origin → `SIGABRT`. This supersedes the earlier `msyscall(2)`
(which marked a single libc text region as the only legal syscall source) and the
`pinsyscall(SYS_execve)` special case. Combined with **xonly** (syscall
instructions live in execute-only text the program can't even read) and
**removal of the generic `syscall(2)` indirection**, the result is: *the only way
to enter the kernel is through the blessed libc stubs, each at its expected
address.*

**Threat it stops.** ROP/JOP chains that try to invoke syscalls directly from
gadget code or attacker-controlled pages — i.e. a post-exploit attacker calling
`execve`/`open` from anywhere other than the real libc stub. It pins the
*provenance* of kernel entry.

**Applicability to oxbow.** Conceptually **very interesting for oxbow**, and only
*partly* redundant. The literal mechanism (pin libc syscall stub addresses)
mostly fights ROP-in-C, which Rust already blunts. But the *principle* —
**"kernel entry / authority invocation must come from a known, blessed origin,
not from arbitrary attacker-redirected control flow"** — maps onto oxbow's
**capability-invocation path**. oxbow already has the stronger version of half of
this: you can't invoke an endpoint without holding the capability, so "calling a
syscall from a gadget" doesn't grant authority the way it does on a UNIX with
ambient syscalls. Where pinsyscalls still adds a thought for oxbow:
- If oxbow's hosted-C/libc path funnels POSIX calls through an oxbow-libc syscall
  stub, **pinning that stub's origin** (and making it xonly) is the same cheap
  hardening, for the same C programs that lack RETGUARD.
- The **xonly + "kernel refuses to `copyin` from a program's own text"** idea
  (OpenBSD blocks syscalls from reading userland text) is a nice
  defense-in-depth against using executable pages as data; cheap to mirror on the
  oxbow syscall/IPC boundary.

**Priority: MED** — adopt the *principle* (blessed, xonly syscall/IPC entry
origin) for the hosted-C libc stub; the native capability path already enforces
the stronger "no cap, no authority" version, so it's redundant there.

### 3.3 RETGUARD in the kernel
Covered in §2.3 — same mechanism, applied to the kernel binary. **REDUNDANT for
oxbow's Rust kernel** (no corruptible return addresses in safe code). Optional
`unsafe`-insurance via Rust stack-protector. **Priority: LOW.**

---

## 4. Randomness — arc4random / kernel CSPRNG / pervasive use

**What it is.** OpenBSD's userland `arc4random(3)` / `arc4random_uniform(3)` is a
**ChaCha20-based CSPRNG** (the name is historical — it dropped RC4 long ago),
seeded from the kernel and **reseeded** periodically and across `fork`. The
kernel has its own CSPRNG feeding `getentropy(2)`. Randomness is wired *pervasively*:
ASLR slides, stack gaps, malloc placement/canaries, the per-boot
`/etc/random.seed`, an `.openbsd.randomdata` ELF section seeded at load,
TCP ISNs, PIDs, etc. The cultural rule: **when you need a random number, use the
good CSPRNG — never `rand()`, never a predictable source**; `srand_deterministic`
exists only to make determinism an explicit, loud choice.

**Threat it stops.** Predictable values anywhere security depends on
unpredictability: defeating ASLR/canaries via guessable layout, predictable TCP
sequence numbers, weak crypto keys/nonces, guessable cookies (`sigreturn`,
`setjmp`). A weak RNG silently undermines *every* randomization-based mitigation
at once.

**Applicability to oxbow.** **This is the most important non-redundant idea in
the whole document, and it is entirely orthogonal to both Rust and capabilities.**
Memory safety does nothing for you if your ASLR slide, your capability **badges**,
your stack-gap, your guard-page placement, or any nonce is drawn from a weak or
unseeded source. Concretely for oxbow:
- Implement a **real kernel CSPRNG** (ChaCha20 or equivalent), properly **seeded
  at boot** from the best available entropy (RDRAND/RDSEED on x86_64, timing
  jitter, virtio-rng if present under Proxmox/QEMU, a persisted seed file), and
  **reseeded** over time. Early-boot entropy is the classic trap — make sure the
  slide and any boot-time secret are drawn *after* seeding, or are reseeded.
- Expose a **`getentropy`-equivalent capability/syscall** as the single blessed
  randomness source for userland (your Rust std port's `getentropy` shim should
  bottom out here; you already have `__oxbow_getentropy`). Make it the *only*
  easy path so nobody rolls their own weak RNG.
- **Capability badges / handle values must be unguessable** where unforgeability
  leans on unpredictability. If a badge or handle can be guessed/forged because
  it's a small counter, the capability model's "unforgeable" claim weakens.
  Audit every place a security property rests on "an attacker can't predict
  this" and route it through the CSPRNG.
- Quality bar: this is *the* place to be paranoid. OpenBSD treats RNG as
  load-bearing infrastructure; oxbow should too.

**Priority: HIGH** — non-redundant, foundational, and it underwrites *every*
randomization-based property oxbow already relies on (ASLR slide, and any
unguessable handle/badge). Get the CSPRNG and its seeding right.

---

## 5. Privilege separation & privilege revocation

**What it is.** A design *pattern*, not a kernel feature: split a program into a
small privileged parent and one or more **unprivileged children** that handle the
risky work (parsing untrusted input, talking to the network). The parent retains
privilege and brokers narrowly; children run as separate users, often `chroot`'d,
communicating over a pipe. **Privilege revocation**: a process that needs root
only to open a resource (raw socket, privileged port) does so, then **drops
privilege** (`setuid` back to the invoking user) before processing input.
OpenSSH's `sshd` is the canonical privsep design; `ping` is the canonical
privdrop. pledge/unveil *reinforce* this (each privsep child pledges to exactly
its job — recall ntpd's three processes each with a tiny pledge).

**Threat it stops.** Containing the blast radius of a compromise: if the
untrusted-input handler is exploited, the attacker lands in a tiny,
de-privileged, namespace-restricted process that can't do much — not in a
root-privileged monolith.

**Applicability to oxbow.** **This is the pattern oxbow is BUILT to express — and
expresses far better than OpenBSD can.** OpenBSD reaches privsep through awkward
UNIX tools (`fork`, separate uids, `chroot`, pipes, `setuid` drop) precisely
because it has ambient authority and a global namespace to claw back. oxbow does
this **natively and unforgeably**:
- "Small unprivileged child holding only what it needs" = a process spawned with
  **only the specific capabilities for its task** (an endpoint to the parent, one
  file cap, one socket cap). There's no uid to drop, no `chroot` to escape — the
  child literally has no handle to anything else.
- "Parent holds privilege and brokers" = the parent holds the broad caps and
  hands children **badged endpoints**; the synchronous-rendezvous IPC is exactly
  the "narrow pipe to the privileged broker."
- "Privilege revocation" = the parent **just doesn't hand over** the cap, or hands
  a one-shot/attenuated cap. Capability **attenuation/revocation** is a
  first-class operation, not a `setuid` hack.

So the *mechanisms* OpenBSD uses for privsep are **REDUNDANT** for oxbow — but the
**discipline is HIGH priority**: oxbow's own servers (net stack, ext2 fs, the
greeter/shell session machinery you've built) should be *designed* as
privsep-style least-authority components. The greeter→shell "greeter asserts,
shell grants" session-channel design in your notes is already textbook
capability-privsep — keep applying that pattern: each server holds the minimum,
hands children badged endpoints, and revokes/attenuates rather than trusting.

A concrete lesson to import: OpenBSD's rule that **the untrusted-input parser is
the most-confined component.** In oxbow, the code paths that parse
attacker-controlled bytes (your from-scratch Ethernet/ARP/IPv4/UDP/TCP parsing,
DHCP, DNS/c-ares, ext2 metadata) should run with the *fewest* caps and be the
*most* isolated servers — because parser bugs are where even Rust's `unsafe` or a
logic error bites hardest.

**Priority: HIGH (as design discipline)** — the mechanism is redundant, the
pattern is central; confine your byte-parsers hardest.

---

## 6. Secure-by-default, minimal attack surface, audit culture

**What it is.** Not a mechanism — a posture. Services off by default; the smallest
possible base system; "every program does one thing well"; a 6–12 person audit
team doing file-by-file analysis since 1996; full disclosure; fix-bugs-even-if-
exploitability-unproven; and the cultural rule **"when we make a security
technology, we apply it to the maximum extent possible"** (no optional hardening
that users disable — contrast Linux/SELinux). LibreSSL and the strlcpy story are
artifacts of "reduce complexity / remove footguns."

**Threat it stops.** Whole-class and unknown-future bugs: a smaller, simpler,
audited, default-locked-down system has fewer places to be wrong and fewer enabled
surfaces to attack.

**Applicability to oxbow.** **oxbow's architecture is this posture made
structural.** "Secure by default" in OpenBSD is a configuration choice; in oxbow
it's the type system — zero ambient authority means the default *is* "no access,"
and you opt *in* by being handed a capability. "Minimal attack surface" is the
microkernel. So much of this is **architecturally REDUNDANT** — but the
*behavioral* parts don't come free and are **HIGH value to adopt as practice**:
- **Apply mitigations to the maximum extent — no opt-outs.** If you enable
  sealed mappings, CET/IBT, or a hardening flag, apply it uniformly. Don't ship
  per-crate escape hatches that erode the invariant.
- **Audit the `unsafe`.** Rust shrinks the audit target to `unsafe` blocks, FFI
  boundaries, and the byte-parsers — but those still need OpenBSD-grade
  file-by-file scrutiny. Maintain an inventory of every `unsafe` block with a
  justification, and treat new `unsafe` as a review trigger. This is your
  highest-leverage audit discipline.
- **Minimize the trusted computing base / kept it small.** Resist pulling large C
  dependencies (you already ported c-ares, lwext4, Lua, tcc) — each is unaudited
  C inside oxbow's trust boundary. Confine them (caps + the privsep discipline of
  §5) and keep them at arm's length.
- **Secure defaults for the cap-granting machinery.** The shell/session/spawn
  layer that mints caps is oxbow's policy brain — its defaults should grant the
  *least* (read-only, narrowest sub-tree, no exec-mapping cap unless asked),
  mirroring "services off by default."

**Priority: HIGH (as practice)** — the architecture gives you the posture; the
audit/uniform-application/TCB-minimization *habits* are what you still have to
do by hand, and they're where real-world oxbow security will be won or lost.

---

## 7. Capability model vs OpenBSD's ambient-authority mitigations — the scorecard

This is the requested clear-eyed mapping. Read the middle column as *"what
OpenBSD is compensating for"* and the right as *"what oxbow does about it."*

| OpenBSD mechanism | Compensating for… | oxbow status | Priority |
|---|---|---|---|
| **pledge(2)** | Ambient syscall authority handed to every process | **Mostly redundant** — no ambient authority to drop. Keep `SYS_PLEDGE` as syscall-class filter for hosted C / defense-in-depth | MED |
| **unveil(2)** | A single global filesystem namespace | **Redundant** — no global namespace; a process can't name what it has no cap for. Keep the *minimization discipline* when minting dir caps | LOW (mech) / HIGH (policy) |
| **W^X** | Machine allows W+X pages | **Already enforced; not language-redundant.** Keep absolute, esp. across tcc/JIT W→X | HIGH |
| **mimmutable(2)** | mprotect can undo W^X post-exploit | **Adopt** — seal mappings one-way; make exec-mapping creation a mintable cap (stronger than OpenBSD) | HIGH |
| **Strict ASLR / stack gap** | Stable, guessable layout in C | Have per-process slide; **add randomized stack gap.** Lower marginal value (Rust removes leak→exploit chain) | MED |
| **RETGUARD (user+kernel)** | Corruptible return addresses in C | **Redundant in safe Rust.** Residual: `unsafe` insurance (Rust stack-protector); tcc output is *unhardened* C — note the gap | LOW (Rust) / MED (hosted C) |
| **Stack protector / SSP** | Stack buffer overflow in C | **Redundant in safe Rust.** Optional `unsafe` insurance | LOW |
| **OpenBSD malloc** (guards/junk/canaries/unmap-on-free) | Heap UAF/overflow in C | **Redundant for safe-Rust heap; valuable in oxbow-libc's C allocator** and for kernel DMA/page-allocator poisoning | MED |
| **MAP_STACK enforcement** | Stack pivoting for ROP in C | **Redundant in safe Rust;** low leverage for hosted C | LOW |
| **strlcpy / linker warnings / bounds checker** | C string footguns | **Redundant** (Rust slices); provide strlcpy in oxbow-libc | LOW |
| **KARL (per-boot relink)** | Large monolithic kernel, stable internal offsets | **Low marginal value** (small microkernel + Rust). Do cheap per-boot whole-image ASLR instead | LOW (relink) / MED (image ASLR) |
| **pinsyscalls / msyscall / xonly entry** | ROP issuing syscalls from gadgets; ambient syscall entry | **Principle worth adopting** for hosted-C libc stub (blessed, xonly origin). Native cap path already enforces stronger "no cap, no authority" | MED |
| **arc4random / CSPRNG / getentropy** | Need for unpredictability everywhere | **NOT redundant — foundational.** Build a real seeded+reseeded ChaCha20 CSPRNG; underwrites ASLR slide + unguessable badges/handles | HIGH |
| **Privilege separation / revocation** | Ambient authority + global namespace; must claw back via uid/chroot/setuid | **Mechanism redundant; pattern central.** Design every server as least-authority; confine byte-parsers hardest; attenuate/revoke caps natively | HIGH (discipline) |
| **Secure-by-default / audit / minimal TCB** | C monolith with optional, disable-able hardening | **Posture is architectural in oxbow.** Still must DO it: audit every `unsafe`, apply mitigations uniformly, minimize C deps in TCB | HIGH (practice) |

### The one-paragraph synthesis
OpenBSD spends most of its engineering re-imposing, at runtime and in hardware,
the two properties oxbow has by construction: **memory safety** (Rust) and **least
authority** (capabilities). Therefore the bulk of OpenBSD's *memory-corruption*
arsenal (RETGUARD, SSP, malloc canaries, MAP_STACK, strlcpy culture) and its
*authority-clawback* arsenal (pledge, unveil, uid/chroot privdrop) are
**redundant for native oxbow code** — and oxbow's versions are *stronger* (a
capability is finer and more unforgeable than a pledge; "no handle" is stronger
than "ENOENT"). The mitigations that **survive** the Rust+capability foundation
are the ones that defend the *machine* and the *math*, not the *language*:
**W^X + sealed/immutable mappings, a real CSPRNG with correct seeding, randomized
layout, and the blessed-origin syscall idea** — plus the language-independent
*disciplines* of **privsep-style server design, hardest-confinement of
byte-parsers, uniform no-opt-out application of mitigations, and auditing every
`unsafe`.** And remember the residue: oxbow *runs a C world* (oxbow-libc, tcc
output) — that world has none of Rust's guarantees, so the "redundant" C
mitigations become **MED-priority again specifically inside oxbow-libc and for
on-device-compiled binaries**, which are oxbow's softest spot (tcc emits no
RETGUARD/SSP).

---

## Sources (primary)
- Beck, *Pledge and Unveil in OpenBSD*, BSDCan 2018 — openbsd.org/papers/BeckPledgeUnveilBSDCan2018.pdf
- de Raadt, *Synthetic Memory Protections* (W^X, xonly, immutable, syscall pinning), csw2023 — openbsd.org/papers/csw2023.pdf
- Bluhm, *OpenBSD Security Mitigations*, EuroBSDcon 2023 — openbsd.org/papers/eurobsdcon2023-bluhm-mitigations.pdf
- `pinsyscalls(2)` man page — man.openbsd.org/pinsyscalls.2
- OpenBSD innovations timeline — openbsd.org/innovations.html (and openbsd-innovations.ctors.net)
- *OpenBSD security features* — Wikipedia (KARL, strlcpy, memory protection)
- OpenBSD Security/Goals/Audit — openbsd.org/security.html
