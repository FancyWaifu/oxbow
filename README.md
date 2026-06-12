# oxbow

A secure-minimal **capability microkernel** written in Rust — an OpenBSD-shaped
security ethos expressed through an seL4-leaned capability ABI. Targets
`x86_64` under QEMU first; the arch-specific code is walled behind
`kernel/src/arch/` so an `aarch64` port stays possible later.

> One line: *an OpenBSD-shaped secure microkernel with an seL4-grade capability
> ABI underneath it, in Rust.*

## Design at a glance

| Axis | Choice |
|---|---|
| Kernel | Microkernel — user-mode drivers/servers, IPC-first |
| IPC | Synchronous call/reply **endpoints** + a flat per-process **handle table** |
| Security | **Zero ambient authority**, W^X always, one attenuation primitive |
| Userland | Rust-only `no_std`; shared `oxbow-abi` + `oxbow-rt` crates |
| Boot | Limine (server binaries ride in as modules) |
| License | BSD-2-Clause |

The kernel ABI is specified — normatively — in [`docs/abi-v0.md`](docs/abi-v0.md).
Read that first; everything in a microkernel is downstream of the IPC/capability ABI.

## Status

**v0 — PONG: complete and verified booting in QEMU.** The kernel boots via
Limine, manages physical and virtual memory under W^X, sets up GDT/TSS/IDT and
the `syscall` fast path, hand-builds one user-mode server (`pong`) from a Limine
module, and runs it in ring 3. The server does one capability-mediated IPC
roundtrip — `sys_call(PING)` → kernel echo → `PONG` printed through a Console
capability — with zero ambient authority. See §7 of the ABI for the normative
acceptance trace. `just run-selftest` additionally exercises every documented
error path (`E_BAD_HANDLE`, `E_RIGHTS`, `E_FAULT`, `E_MSG`, `E_NOSYS`) from ring 3.

Built in nine verified phases (physical memory → CPU tables → paging → ring-3
syscall → module plumbing → ELF loader → capabilities → IPC → hardening).

**v1 arc 1 — threads + preemptive scheduler: complete.** A PIT timer (IRQ0 via
the remapped 8259 PIC) drives a round-robin scheduler over a fixed pool of
kernel threads. The kernel is non-preemptible (IF=0 in all kernel code);
preemption lands only in ring 3 (IF=1) and at idle `sti; hlt` points. The user
process runs as a schedulable thread — preempted mid-userspace, concurrent with
kernel threads — and `sys_exit` kills the thread, not the machine.

**v1 arc 2 — per-process address spaces + isolation: complete.** Each process
gets its own PML4 (sharing the kernel upper half), and the scheduler reloads CR3
when dispatching a thread in a different address space. Two user processes
(`pong`, `beta`), both linked at `0x200000`, run concurrently in *separate*
address spaces — same vaddr, different memory. A ring-3 fault kills the
offending thread and its process while everything else continues (`just
run-faulttest`); `just run-isolation` shows two processes reading different bytes
at the same address and a hostile one dying alone. User-to-user IPC is still the
kernel echo (arc 3).

### Next (v1, later arcs)

Blocking user-to-user IPC (real `sys_recv`/`sys_reply`, pooled Reply objects,
rendezvous + handle transfer — the kernel echo retired); user-driven memory
(untyped/retype, `sys_map`); IRQ capabilities for real drivers; and the
`aarch64` port (the `arch/` wall is already in place for it).

## Building & running

Requires: a nightly Rust toolchain (pinned via `rust-toolchain.toml`),
`qemu-system-x86_64`, `xorriso`, `just`, and the Limine bootloader binaries.
The Limine path is configured in the `justfile` (`LIMINE_DIR`).

```sh
just run           # build kernel -> assemble ISO -> boot in QEMU (serial on stdio)
just run-selftest  # same, but run the ABI negative-path selftests first
just build         # just compile the kernel
just iso           # build the bootable ISO
just gdb           # boot under QEMU stopped, waiting for gdb on :1234
just clean
```

## Layout

```
kernel/   the microkernel (no_std, no_main; boots via Limine)
abi/      oxbow-abi — syscall numbers, rights, errors, MsgBuf (shared kernel+user)
rt/       oxbow-rt  — userland runtime: _start, syscall stubs, panic handler
servers/  user-mode servers (the pong server is the first; added with v0)
tools/    build-time helpers (initrd packer, etc.)
docs/     abi-v0.md — the normative capability/IPC ABI
```
