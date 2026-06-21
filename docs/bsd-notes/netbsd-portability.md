# NetBSD Portability Engineering — Reference Notes for oxbow

Research notes on how NetBSD achieves portability across ~50+ architectures,
mapped onto oxbow (Rust capability microkernel, currently x86_64-only). The
goal is a concrete MI/MD discipline to adopt **now**, before a 2nd arch
(aarch64) lands, plus the observation that oxbow's microkernel structure is
already an "anykernel."

Sources: netbsd.org/about/portability.html, the rump-kernel papers (Kantee
BSDCan'09, Cormack AsiaBSDCon'15), Thorpe's "A Machine-Independent DMA
Framework for NetBSD" (USENIX'98), bus_space(9), the NetBSD build.sh guide,
and the pkgsrc guide. oxbow structure facts come from a source-tree audit
(see paths inline).

---

## Top takeaways for oxbow (exec summary)

1. **oxbow is already ~94% portable and didn't have to try.** Rust + the
   microkernel structure do for free what NetBSD spent decades engineering in
   C. The source audit shows only ~1,500 of ~26,500 LOC (5.6%) is
   machine-dependent, and **all** of it is in the kernel — the 44 userspace
   servers, libc, and the ABI crate are 0% MD. The single biggest portability
   asset oxbow has is structural, not code: subsystems already run as
   userspace servers reached by IPC.

2. **oxbow's microkernel *is* a built-in anykernel / rump structure.** NetBSD
   worked hard to lift its fs / net / driver code *out* of the kernel so it
   could run as userspace libraries (rump kernels) against a thin "hypercall"
   platform layer. oxbow's `servers/` (net stack, `fsd` ext2, `gpu`) already
   live in userspace and talk to hardware only through capability handles. The
   "hypercall layer" NetBSD had to invent is, for oxbow, just the syscall ABI +
   the capabilities the kernel hands a server. Porting a server to a new arch =
   re-granting the same capabilities; the server code does not change.

3. **The one real porting cost is the kernel, and it's already walled off —
   but the wall leaks in 6 files.** `kernel/src/arch/mod.rs` is a textbook
   NetBSD-style MI/MD wall (`#[cfg(target_arch)]` picks a backend; the rest of
   the kernel uses only re-exported names). But x86_64 assumptions still leak
   *outside* arch/ into `mm/vm.rs`, `usermem.rs`, `percpu.rs`, `smp.rs`,
   `pci.rs`, `rng.rs`. Tightening those 6 files into the arch trait is the
   single highest-value pre-aarch64 task.

4. **Adopt NetBSD's two killer abstractions, sized down: a `pmap`-style MMU
   trait and a `bus_space`/`bus_dma`-style I/O capability.** NetBSD shares one
   driver across PCI on Alpha/i386/macppc/arm because CPU↔device access
   (`bus_space`) and device↔memory access (`bus_dma`) are abstracted behind
   tag+handle types. oxbow's servers already receive pre-mapped MMIO/DMA
   capabilities from the kernel — that *is* a bus_space/bus_dma analog. Make it
   explicit (a small `BusSpace`/`DmaRegion` wrapper type in `oxbow-rt`) so
   drivers stop doing raw `read_volatile` on a vaddr and gain MMIO barriers +
   DMA cache-sync hooks that aarch64 will need but x86 won't.

5. **The build system and packaging are model citizens to copy later, not
   now.** build.sh cross-builds the whole OS for any arch from any POSIX host,
   unprivileged, via a self-bootstrapped toolchain — oxbow already gets the
   cross-build for free (Rust + a custom target JSON). pkgsrc's "separate base
   from packages, relocatable prefix, unprivileged install" model is the right
   template for oxbow app distribution (it maps cleanly onto the
   `/bin` system-tools vs per-user-home capability split oxbow already has),
   but it's a LOW priority until there's a 2nd arch and >1 user workflow.

---

## 1. The MI / MD source split (`sys/kern` vs `sys/arch/<arch>`)

**What it is.** NetBSD splits every line of the kernel into *Machine
Independent* (MI) and *Machine Dependent* (MD) trees. `sys/kern`, `sys/net`,
`sys/dev` (the MI device drivers), `sys/vfs`, `sys/uvm` are written once and
compiled for every architecture. `sys/arch/<arch>/` holds only the
irreducibly machine-specific code: early boot, trap/interrupt vectors, the
`pmap` (physical-map / MMU) implementation, context switch, and MD bus
attachment glue. The canonical example: the `fxp(4)` Ethernet driver is one MI
core driver in `sys/dev/`, matched at runtime with MD PCI/Cardbus attachment
code, and the *same* driver runs on alpha, i386, macppc, cats, cobalt, prep —
"written once, used many times."

**Portability insight.** The split is enforced by *discipline plus a hard
directory boundary*, not by language. MI code may never `#include` an
arch header or assume endianness, register width, page size, or MMU layout;
anything machine-specific is reached through an MI interface (`pmap_*`,
`bus_space_*`, `cpu_switchto`, `curcpu()`). Porting to a new arch becomes
"fill in `sys/arch/<newarch>/`" rather than "edit the whole kernel." Because
the same MI code runs on wildly different machines, bugs surface fast (a
latent assumption that holds on i386 breaks loudly on big-endian sparc).

**Applicability to oxbow.**
- *Already has it:* `kernel/src/arch/mod.rs` is exactly the NetBSD wall —
  ~1,089 lines of doc + a `#[cfg(target_arch = "x86_64")] mod x86_64;` plus a
  `pub use x86_64::{...}` re-export of ~22 names (`context_switch`, `io_in/out`,
  `load_cr3`, `enter_user`, `set_fs_base`, `timer_init`, etc.). The rest of the
  kernel is supposed to use only those names. This is *better* than NetBSD's
  C-preprocessor approach because Rust's module system + `cfg` makes the wall a
  compile error to bypass (if you isolate properly).
- *Gap to close:* the wall leaks. Six MI files reach around it and touch
  x86_64 directly:
  - `kernel/src/mm/vm.rs` (~478 LOC) — uses the `x86_64` crate's
    `structures::paging` (PML4/PD, PRESENT/WRITABLE/NO_EXECUTE), `Efer`. This is
    oxbow's `pmap`, and right now it *is* the MD pmap masquerading as MI code.
  - `kernel/src/usermem.rs` — walks the live PML4 via `Cr3` to validate user
    pointers, with hardcoded 4-level/9-bit-index/2MiB-huge assumptions.
  - `kernel/src/percpu.rs` — per-CPU data via `gs:[...]` inline asm + the
    `IA32_GS_BASE` MSR.
  - `kernel/src/smp.rs` — `__cpuid`, AP stack-switch asm.
  - `kernel/src/pci.rs` — PCI config via 0xCF8/0xCFC port I/O (x86-only
    mechanism; aarch64 uses ECAM/MMIO).
  - `kernel/src/rng.rs` — `rdtsc`/`rdseed`/`rdrand` + `__cpuid`.
- *Concrete fix (the MI/MD discipline to adopt now):*
  1. Define the arch boundary as a **trait**, not just a re-export list.
     Create `kernel/src/arch/mod.rs::trait Arch` (or a set of free functions
     behind `cfg`) covering: MMU/pmap ops, per-CPU base get/set, PCI config
     read/write, entropy source, TLB/barrier ops, CPU id. Each arch module
     implements it. Today's re-export list is the informal version of this;
     promote it to a named contract so the compiler lists exactly what a new
     arch must provide.
  2. **Move the pmap.** Lift `mm/vm.rs` and `usermem.rs`'s page-table logic
     behind an arch `Pmap` interface: `map(va, pa, flags)`, `unmap(va)`,
     `translate(va)`, `validate_user(va, len, access)`, `new_address_space()`,
     `switch_to(asid)`. x86_64 implements it with PML4; aarch64 implements it
     with TTBR0/TTBR1. The *callers* (syscall layer, IPC, server bring-up)
     speak only VA/PA/flags. This is the single most important refactor.
  3. **Abstract page size.** Replace the scattered literal `4096` /
     `const PAGE: u64 = 4096` (in `usermem.rs:13`, `mm/vm.rs:19`, `syscall.rs`,
     `rt/src/lib.rs:72` heap base) with one `arch::PAGE_SIZE` const. aarch64
     supports 4K/16K/64K granules; baking 4096 everywhere is a latent bug.
  4. **Move `pci.rs`, `percpu.rs`, `rng.rs`, `smp.rs` bodies into
     `arch/x86_64/`** and expose MI signatures (`pci_config_read(bdf, off)`,
     `percpu_base()`, `entropy_fill(&mut [u8])`, `cpu_id()`).

**Priority: HIGH** — this is *the* porting cost; the wall exists but the 6
leaks must be sealed before any aarch64 work, and the pmap trait is the
keystone.

---

## 2. `bus_space` / `bus_dma` — one driver, many buses and arches

**What it is.** Two MI abstractions that let a single device driver run
unmodified across architectures and bus types:
- **`bus_space`** = CPU-to-device access. A driver never does `inb`/`outb` or
  a raw MMIO pointer deref. It gets a `bus_space_tag_t` (opaque, identifies the
  bus/arch access method — port I/O vs MMIO vs some bridge) and a
  `bus_space_handle_t` (a mapped region), then calls
  `bus_space_read_4(tag, handle, offset)` / `bus_space_write_4(...)`. On i386 a
  tag might mean `inl`; on a RISC machine it means a volatile load at a mapped
  address; the driver doesn't know or care. The tag/handle pair also carries
  endianness/stream semantics and memory barriers (`bus_space_barrier`).
- **`bus_dma`** = device-to-memory access (DMA). Thorpe's USENIX'98 paper:
  different machines map DMA totally differently — i386 is WYSIWYG (device sees
  the same physical address the CPU does), Alpha uses a direct-mapped window at
  an offset, others have an IOMMU/SGMAP that *remaps* and may bounce. The
  driver allocates a `bus_dmamap_t`, calls `bus_dmamap_load(map, buf, len)`,
  and reads back a list of *device-visible* `bus_addr_t` segments to program
  into the hardware — never assuming a kernel VA equals a device address. It
  calls `bus_dmamap_sync(PREREAD/POSTREAD/PREWRITE/POSTWRITE)` to handle cache
  flush/invalidate and bounce-buffer copies.

**Portability insight.** The hard, machine-specific facts — is there an IOMMU?
does the bus remap addresses? is the cache coherent with DMA? what's the
barrier instruction? — are *captured in the tag object at attach time* and
hidden behind a uniform call interface. The driver expresses *intent*
("read register 4", "this buffer must be DMA-visible, sync it before the device
reads") and the MD backend supplies *mechanism*. This is why NetBSD shares PCI
drivers across nine CPU families.

**Applicability to oxbow.**
- *Already has the structure, implicitly:* oxbow servers do **not** do port
  I/O or pick physical addresses. The `net` server (e1000) receives a
  `BOOT_PCI` cap, a pre-mapped `NET_MMIO` region (a vaddr), `NET_DMA` buffers
  pre-allocated by the kernel, and a `BOOT_NET_IRQ` cap. The `gpu` server
  (virtio-gpu) likewise gets `GPU_MMIO`/`GPU_DMA` caps. So oxbow already does
  the two things bus_space/bus_dma exist to guarantee: (a) the driver touches
  MMIO only through a kernel-granted mapping, and (b) the driver gets
  DMA buffers from the kernel rather than computing physical addresses. The
  capability-passing kernel *is* the bus_space tag.
- *Gap (matters specifically for aarch64):* servers currently do raw
  `read_volatile`/`write_volatile` on the MMIO vaddr and treat DMA buffers as
  plain memory. That works on x86 because x86 MMIO is strongly ordered and DMA
  is cache-coherent. **aarch64 is not** — it needs explicit DMB/DSB barriers
  around MMIO ordering and cache maintenance (clean/invalidate) around
  non-coherent DMA. If you add aarch64 without an abstraction, every driver
  silently corrupts data.
- *Concrete fix:* introduce two thin wrapper types in `oxbow-rt` (or a shared
  `oxbow-bus` crate) that every driver uses instead of raw pointers:
  - `struct Mmio { base: *mut u8 }` with `read32(off)`, `write32(off, v)`,
    `barrier()` — on x86 these compile to plain volatile ops; on aarch64
    `barrier()` emits DMB and the accessors can force ordering. This is
    `bus_space`.
  - `struct DmaRegion { va, dev_addr, len }` with `sync_for_device(range)` /
    `sync_for_cpu(range)` — no-ops on x86, cache ops on aarch64. Crucially it
    exposes `dev_addr` (the address to program into hardware) **separately**
    from the CPU `va`, so the day oxbow runs behind an IOMMU or on a
    non-WYSIWYG bus, only the kernel's DMA-cap minting changes, not the driver.
    This is `bus_dma`.
  Retrofit `net` and `gpu` to use these now (while they're the only two
  drivers) so the pattern is set before the driver count grows.

**Priority: HIGH** — cheap now (2 drivers), and without it the first aarch64
boot will have silent DMA/MMIO-ordering corruption that is brutal to debug.
The `dev_addr`-separate-from-`va` discipline is the load-bearing part.

---

## 3. autoconf / config(5) — device-tree and driver attachment

**What it is.** NetBSD's `config(8)` reads a per-port kernel config file
(`sys/arch/<arch>/conf/<NAME>`) describing which drivers to include and the
bus topology, and generates C tables. At boot, the `autoconf(9)` framework
walks the bus tree top-down: a parent bus *probes* for children, and for each
candidate calls every plausible driver's **match** routine; the best match's
**attach** routine binds the driver to the device. Drivers are registered as
`cfattach` records keyed by *(driver, parent-bus)* with *locators* (e.g. PCI
bus/dev/function, or fixed ISA I/O ports). Output is the familiar dmesg tree:
`fxp0 at pci0 dev 5 function 0`. A driver can attach at multiple parents
(PCI *and* Cardbus) by registering multiple cfattach records over one core.

**Portability insight.** Device discovery is *data-driven and recursive*, not
hardcoded. The MI core driver declares "I attach to a PCI device with these
IDs"; the MD bus backend supplies the actual enumeration mechanism (PCI config
space scan on x86, device-tree/ACPI on arm/arm64). The same match/attach
contract works whether children are found by probing legacy ports, scanning
PCI, or reading a flattened device tree — so the *driver* is arch-agnostic
while *discovery* is per-arch/per-bus.

**Applicability to oxbow.**
- *Partly has it, in the kernel:* `kernel/src/pci.rs` enumerates NIC/disk/GPU
  on the PCI bus and the kernel then mints the right MMIO/DMA/IRQ caps to the
  matching server. That kernel-side enumeration + cap-granting is oxbow's
  proto-autoconf: discovery (MD) is separated from the driver (MI server).
- *Gap:* the matching is hardcoded (boot wires e1000→net, virtio-gpu→gpu).
  There's no data-driven registry of "server X handles device class Y," and
  discovery is x86-PCI-only. aarch64 platforms typically enumerate via a
  flattened **device tree (FDT)** or ACPI, not PCI config ports.
- *Concrete fix (low effort, do when adding a 2nd device or 2nd arch):*
  1. Define a small in-kernel **bus enumeration interface**:
     `trait BusEnumerator { fn probe(&self) -> Vec<DeviceInfo> }` with a
     `PciEnumerator` (x86) and later an `FdtEnumerator` (aarch64).
     `DeviceInfo` = {class/vendor/device id, MMIO ranges, IRQ, DMA window}.
  2. Replace the hardcoded device→server wiring with a tiny **match table**
     (vendor/device id → server name + which caps to mint). This is the
     `cfattach` analog: it lets you add a driver by adding a table row, and it
     makes the x86-vs-aarch64 difference live entirely in the enumerator.
  Don't over-build this — oxbow has 3 devices, not 3,000. A `match` on
  (vendor, device) feeding a cap-minting routine is enough; the value is
  isolating *discovery mechanism* (MD) from *device→driver policy* (MI).

**Priority: MED** — not on the critical path for a first aarch64 boot (you can
hardcode virt's devices), but the PCI-only assumption in `pci.rs` *is* MD and
the FDT path is needed for real arm hardware. Do the enumerator trait alongside
the `pci.rs`→arch/ move from §1.

---

## 4. rump kernels / the anykernel — subsystems as userspace libraries

**What it is.** The headline-relevant one. NetBSD's **rump kernel** (Antti
Kantee's PhD work, in-tree since NetBSD 5.0, 2009) runs *unmodified real
kernel code* — the actual fs, network stack, and device drivers — as ordinary
userspace libraries. The insight (Kantee): apart from low-level MD code and a
few privileged operations, kernel subsystems don't care whether they run in
ring 0; the only thing stopping them is that kernel modules depend on other
kernel modules. So rump provides the *missing* dependencies for userspace and
omits what it can borrow from the host: a rump kernel **does not** implement
its own memory allocator, threads, or scheduler — those come from the
**platform** via a thin **`rumpuser` hypercall interface** (memory, threads,
clocks, I/O). The **anykernel** concept generalizes this: the same kernel
component drivers can be composed to run in a monolithic kernel, in userspace
as a library, on bare metal, on Xen, or inside a microkernel — "anykernel"
because the code isn't committed to one kernel architecture. Rumprun later
turned rump kernels into unikernels.

**Portability insight.** Decouple *subsystem logic* from *kernel mechanism*.
A driver/fs/netstack written against a narrow, well-defined platform interface
(alloc, threads, clocks, hypercalls for real I/O) can be hosted anywhere that
implements that interface. The hard MD/privileged surface shrinks to the
hypercall layer; everything above it is portable by construction. This is the
same lever as the MI/MD split, but applied to *where the code runs* rather
than *what arch it targets*.

**Applicability to oxbow — this is the big one.**
- **oxbow's microkernel is structurally an anykernel already, and its servers
  are already "rumped."** NetBSD had to invent rump to get its fs/net/driver
  code running outside ring 0 against a thin platform layer. In oxbow that's
  not a retrofit — it's the *native* design. `servers/net`, `servers/fsd`
  (ext2/lwext4), `servers/gpu` are real subsystems running in userspace as
  separate processes. The audit confirms they contain **zero** arch-specific
  code and depend only on `oxbow_rt::sys_*` + `oxbow_abi::*`. The `rumpuser`
  hypercall interface that NetBSD bolted on is, for oxbow, *just the syscall
  ABI plus the capabilities the kernel grants* — memory via `sys_map`, threads
  via the scheduler, I/O via MMIO/DMA/IRQ caps. oxbow gets rump's benefits
  (subsystem isolation, a crash = one server restart not a kernel panic,
  test-a-subsystem-in-isolation, develop fs/net without touching the kernel)
  *for free and by default.*
- **What to borrow anyway — name and harden the hypercall interface.** Rump's
  discipline is that the platform interface is *small, explicit, and the only
  thing a subsystem may assume.* oxbow should treat its server-facing ABI the
  same way: enumerate exactly the syscalls + capability kinds a server is
  allowed to depend on, and forbid anything else. That enumerated surface is
  what you re-implement per arch — and since it's already arch-clean, a server
  *binary* compiled for `x86_64-unknown-oxbow` differs from an
  `aarch64-unknown-oxbow` one only in codegen, not logic.
- **Borrow the testability story explicitly.** Rump's biggest practical win is
  "boot a subsystem in 10ms in a normal process, run it under gdb/valgrind."
  oxbow can do the analog: because servers are normal userspace programs
  talking IPC, you could host a server against a *mock* IPC/cap backend on the
  dev host (or under QEMU-user) to unit-test the ext2 server or net stack
  without booting the whole OS. Worth a small `oxbow-rt` "hosted" shim (you
  already built a `hosted` feature for the Rust std port — same idea).

**Priority: HIGH (conceptual) / LOW (work needed).** The architecture is
already done — this is mostly *recognizing and protecting* what you have:
keep the server-facing ABI minimal and arch-clean, and don't let a future
"fast path" smuggle arch assumptions or raw physical addressing into a server.
The single rule: **a server may only depend on the syscall ABI and the caps it
is granted — never on the arch, never on a physical address it computed
itself.** That rule is what makes oxbow's anykernel property durable.

---

## 5. build.sh / tools / nbmake — cross-build the whole OS from any host

**What it is.** One script, `build.sh`, builds the entire OS — kernel +
userland + bootable image — for any of ~50 architectures, from any
POSIX host (Linux, macOS, NetBSD…), as an unprivileged user. Its first act is
to **build its own toolchain** into `$TOOLDIR`: it bootstraps `nbmake`
(NetBSD's `make`), then cross-compiler, assembler, linker, and `config`. The
host only needs a C/C++ compiler. Everything downstream uses the freshly built
tools, so the build is hermetic and reproducible regardless of host quirks.
`./build.sh -U -a aarch64 -m evbarm tools release` cross-builds an entire
aarch64 release on an amd64 box, no root.

**Portability insight.** Don't depend on the host's tools or privileges —
*bootstrap your own*. By owning the toolchain and never writing outside the
object/dest dirs, the build is decoupled from the host OS, so "build for arch
X" is a flag, not a separate procedure. Native and cross builds are literally
the same code path (cross is the general case; native is cross-to-self).

**Applicability to oxbow.**
- *Already has it, courtesy of Rust:* cross-compilation is a target triple.
  oxbow has `x86_64-unknown-oxbow.json` (custom target spec), `.cargo/config`
  with `build-std`, and a `justfile`. Adding aarch64 = add
  `aarch64-unknown-oxbow.json` and a `just` recipe; Rust/LLVM *is* the
  self-contained cross toolchain NetBSD had to build by hand. You already get
  "unprivileged, hermetic, cross from any host."
- *Gaps to watch:* (1) the build bakes some arch into flags — `.cargo/config`
  pins `target = "x86_64-unknown-none"`, `code-model=kernel` (x86-specific
  code model), and some servers force `+sse,+sse2` for C interop. Parameterize
  these per-target so the aarch64 recipe doesn't inherit x86 code models /
  SSE. (2) `linker.ld` and the higher-half address (0xFFFF_8000_…) are x86_64
  layout — aarch64 wants its own linker script + TTBR split. Keep a
  per-arch linker script (`linker-x86_64.ld`, `linker-aarch64.ld`), selected
  by the `just` recipe, mirroring `sys/arch/<arch>/conf`.
- *Borrow:* nothing structural to build — just keep arch-specific build knobs
  out of the shared `.cargo/config` and into per-target files, the same way
  NetBSD keeps them in `sys/arch/<arch>/`.

**Priority: MED** — small, mechanical, but must be done as part of the aarch64
bring-up (per-arch target JSON + linker script + code-model). Not a research
problem; just don't let x86 flags leak into shared config.

---

## 6. sysctl — uniform MI/MD configuration interface

**What it is.** A single hierarchical namespace (`kern.*`, `vm.*`, `net.*`,
`hw.*`, `machdep.*`) for reading and tuning kernel state at runtime, via one
`sysctl(8)` tool and `sysctl(3)` call. The tree is mostly MI (`kern.hostname`,
`net.inet.ip.forwarding`), with a dedicated `machdep.*` subtree for the
machine-dependent knobs. New nodes register into the same tree; userland sees a
uniform interface regardless of which subsystem or arch owns the leaf.

**Portability insight.** *One interface, namespaced by ownership.* MI
subsystems and MD code both publish into the same tree; the MD-ness is
contained to a named subtree (`machdep.*`) instead of leaking into bespoke
per-arch config tools. Userland and scripts stay arch-agnostic.

**Applicability to oxbow.**
- oxbow has no equivalent yet (config is ad-hoc / compile-time). When
  runtime introspection/tuning is wanted, copy the *shape*, not the
  byte-protocol: a capability-mediated, hierarchical key/value query served by
  the relevant server (the kernel serves `kern.*`/`machdep.*`, the net server
  serves `net.*`). The key portability lesson is just the `machdep.*`
  convention: when an aarch64-only or x86-only knob appears, namespace it so MI
  tooling never special-cases arch.
- This also dovetails with oxbow's capability model nicely — a sysctl-like
  query is just another IPC method, and *which* subtree you can read/write is a
  capability.

**Priority: LOW** — not a portability blocker; adopt the `machdep.*`
namespacing convention if/when a runtime config interface is built.

---

## 7. pkgsrc — portable packaging as a model for oxbow app distribution

**What it is.** NetBSD's package system, but deliberately built to run on
*many* OSes (NetBSD, Linux, macOS, Solaris, …). Forked from FreeBSD ports in
1997 with portability as the explicit goal. Each package is a directory with a
`Makefile` (metadata + build recipe), `distinfo` (checksums), and patches.
A `bootstrap` script stands pkgsrc up on a foreign host. Two principles matter
for oxbow: (1) **base vs packages separation** — the OS base system (`/bin`,
`/lib`) is *only* the core OS; everything else installs under a separate
relocatable prefix (`/usr/pkg`, or `~/pkg` unprivileged), so packages never
pollute the base. (2) **relocatable, unprivileged prefix** — a user can have a
self-contained userland in their home dir, isolated from other users.

**Portability insight.** Separate *the OS you ship* from *the software people
add*, behind a relocatable prefix, and make the package format
host-abstracted (a thin bootstrap layer hides OS differences). The result is
one package tree usable across many systems and many users without root.

**Applicability to oxbow.**
- **This maps almost 1:1 onto a split oxbow already made.** Per the project
  notes, oxbow chose "Option A": `/bin` holds shared *system tools* reachable
  by every user via a dir capability, while anything outside `/bin` + your home
  is unreachable (no cap = no access). That *is* pkgsrc's base-vs-packages
  separation, enforced by capabilities instead of path convention. A user's
  installed apps living under their home (a per-user relocatable prefix they
  hold a cap to) is exactly pkgsrc's `~/pkg` unprivileged model — except
  oxbow's isolation is structural (capability confinement), not advisory.
- *Borrow when building app distribution:* keep system tools in `/bin`
  (the cap-shared base), install user apps under a per-user prefix in home,
  and make app packages *relocatable* (no hardcoded absolute paths) so the same
  artifact works regardless of which user/prefix installs it. Since oxbow apps
  are already "write C → `cc src.c -o /bin/prog`" or per-home binaries, a
  package = a relocatable bundle + a manifest of caps it needs. The cap-need
  manifest is oxbow's improvement over pkgsrc: a package declares the
  capabilities it requires and the user grants them at install — least
  authority by construction.

**Priority: LOW** — distribution is a later-stage concern; oxbow already has
the structural split that makes pkgsrc-style packaging natural. Note it as the
template and move on.

---

## 8. The MI device-driver model and bringing up a new architecture

**What it is (synthesis).** Putting §1–3 together, NetBSD's recipe to add an
arch: create `sys/arch/<arch>/` with (a) early boot + trap/interrupt vectors,
(b) the `pmap` MMU implementation, (c) `cpu_switchto` context switch + clock,
(d) MD bus front-ends (`bus_space`/`bus_dma` tag implementations) and a bus
enumerator, (e) a `conf/` kernel config. Then *every existing MI driver in
`sys/dev/` works unchanged*, because it only ever spoke `bus_space`/`bus_dma`/
autoconf. The driver is MI; only its attachment and the platform are MD.

**Applicability to oxbow — the concrete aarch64 bring-up plan.** Mirroring the
NetBSD recipe, sized for oxbow:

1. **`kernel/src/arch/aarch64/`** implementing the same contract as
   `arch/x86_64/` (the §1 trait): early boot (EL1 setup), exception vectors
   (instead of IDT), `context_switch` (different register file), per-CPU base
   (use `TPIDR_EL1` instead of GS), timer (the ARM generic timer, not PIT/APIC),
   serial (PL011 UART, not 16550), entropy (`RNDR`/timer fallback, not RDRAND).
   This is the bulk of the work; ~1,400 LOC of x86 has a structural twin here.
2. **The pmap.** Implement the §1 `Pmap` trait with TTBR0 (user) / TTBR1
   (kernel) and aarch64 descriptor formats. Page-table *structure* is
   4-level like x86; only bit layouts/flags differ. `usermem.rs`'s validation
   walk moves behind `Pmap::validate_user`.
3. **Syscall path.** `rt/src/lib.rs` syscall stubs use `svc #0` instead of
   `syscall`; kernel entry uses the synchronous-exception vector instead of
   the `LSTAR`/`SYSCALL` MSR path in `arch/x86_64/syscall.rs`. The syscall
   *numbers and ABI* (in the `abi` crate) stay identical — only the
   instruction + entry glue change.
4. **bus_space/bus_dma (§2).** Implement `Mmio::barrier()` = DMB/DSB and
   `DmaRegion::sync_*` = cache clean/invalidate for the aarch64 backend. This
   is where x86's "do nothing" becomes real work. Drivers (`net`, `gpu`)
   don't change.
5. **Bus enumeration (§3).** Add an FDT enumerator for the QEMU `virt`
   machine (which describes devices via a flattened device tree, not PCI
   config ports). For a first boot you can even hardcode `virt`'s known MMIO
   addresses, then generalize.
6. **Build (§5).** `aarch64-unknown-oxbow.json` (drop `code-model=kernel`,
   pick the aarch64 code model + features), `linker-aarch64.ld`, a `just`
   recipe. Servers/libc/rt recompile unchanged for the new triple.
7. **What you DON'T touch:** all 44 servers, libc, the `abi` crate, and the
   IPC/scheduler MI core — *if* §1's leaks are sealed first. That's the payoff
   of doing §1 before §8.

**Priority: HIGH** (this is the actual goal) — but gated on §1 (seal the
leaks, extract the pmap trait) and §2 (bus_space/bus_dma wrappers) being done
first, or the port turns into a whole-kernel edit instead of a
fill-in-`arch/aarch64/` exercise.

---

## Priority summary

| # | Technique | oxbow already has? | Priority | One-line justification |
|---|-----------|--------------------|----------|------------------------|
| 1 | MI/MD split (`arch/mod.rs` wall) | Yes, but wall leaks in 6 files | **HIGH** | The only real porting cost; seal leaks + extract a pmap trait before aarch64. |
| 2 | bus_space / bus_dma | Implicitly (MMIO/DMA caps) | **HIGH** | Cheap now (2 drivers); without it aarch64 DMA/MMIO-ordering silently corrupts. |
| 3 | autoconf / config(5) | Partly (kernel PCI enum + cap grant) | **MED** | PCI-only enum is MD; aarch64 needs an FDT enumerator + a match table. |
| 4 | rump / anykernel | **Yes — natively** (servers in userspace) | HIGH concept / LOW work | Recognize & protect it: keep server ABI minimal and arch-clean. |
| 5 | build.sh cross-build | Yes (Rust target triple) | **MED** | Just keep x86 flags (code-model/SSE/linker) out of shared config. |
| 6 | sysctl | No | **LOW** | Adopt the `machdep.*` namespacing convention if a runtime config iface appears. |
| 7 | pkgsrc | Structurally (base `/bin` vs per-home caps) | **LOW** | Right template for app distribution; not a portability blocker. |
| 8 | MI driver model / new-arch bringup | Mostly (MI servers) | **HIGH** | The goal; gated on #1 and #2 being done first. |

---

## The one rule to internalize

NetBSD's portability is not magic — it's *discipline at a boundary*: MI code may
only speak MI interfaces (`pmap`, `bus_space`, `bus_dma`, autoconf), and every
machine fact is captured in an object/tag at attach time. oxbow gets most of
this free from Rust + its microkernel structure, and its userspace servers are
already an anykernel. The remaining work is to **make oxbow's boundaries
explicit and leak-free**: a `Pmap`/`Arch` trait in the kernel, a
`bus_space`/`bus_dma`-style MMIO/DMA wrapper for drivers, and a standing rule
that **a server depends only on the syscall ABI and the capabilities it is
granted — never on the architecture, never on a physical address it computed
itself.** Do that, and aarch64 becomes "fill in `arch/aarch64/`," exactly as it
is for NetBSD.
