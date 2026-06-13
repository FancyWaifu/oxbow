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
when dispatching a thread in a different address space. Two user processes,
both linked at `0x200000`, run concurrently in *separate* address spaces. A
ring-3 fault kills the offending process while everything else continues.

**v1 arc 3 — blocking user-to-user IPC: complete.** The kernel echo is gone:
`pong` (pinger) and `beta` (ponger) do a real PING→PONG rendezvous across two
address spaces. A sender that arrives first BLOCKS on the endpoint's send queue;
a receiver that arrives first blocks as its `recv_waiter`; whoever completes the
rendezvous wakes the other. Messages cross address spaces via per-thread kernel
STAGING (each thread only ever touches its own user memory, under its own CR3).
Reply is a real pooled kernel object handed to the receiver as a handle;
`sys_reply` consumes it; a replier that dies mid-call wakes the caller `E_GONE`.
Capabilities transfer across the rendezvous (`pong` grants an attenuated console
to `beta`, which writes through it). Both arrival orderings, `E_GONE`, and the
selftest 7/7 all verified.

**v1 arc 4 — user-driven memory: complete.** Each process is born holding a
`Memory` capability — a byte budget (a degenerate seL4 "untyped"). `sys_map`
takes that handle and debits it to map anonymous pages into the caller's own
address space, charging intermediate page-tables too; exhaustion is `E_NOMEM`.
**Law L6 is now literally enforced** — the kernel never allocates a user frame
without an authorizing capability, and a process can only consume what it was
granted. `Frame` objects name a physical frame; because handles transfer over
IPC, a frame can be **shared zero-copy** between two isolated address spaces,
with read-only sharing falling out of capability attenuation (a writable map
through a read-only handle is `E_RIGHTS`). See `docs/abi-v0.md` §9.

**v1 arc 5 — IRQ capabilities + a user-mode keyboard driver: complete.** A user
process (`servers/kbd`) is a real device driver: it holds the keyboard IRQ line
and the i8042 I/O ports as **capabilities**, binds the IRQ to a **Notification**
(an async signal the kernel's handler can fire without blocking), waits on it,
reads scancodes via `sys_io_in`, translates them, and forwards each keystroke to
the TTY. The kernel never touches the keyboard. See `docs/abi-v0.md` §10.
(Headless test: `just`-style boot + QEMU monitor `sendkey`.)

**v1 arc 6 — a TTY + an interactive shell: complete.** Three userspace processes
form a terminal over one tag-multiplexed endpoint (no new syscalls). `kbd` posts
keystrokes; **`tty`** (`servers/tty`) is the sole receiver and the sole Console
writer — it runs the line discipline (echo, backspace rub-out, buffer-to-Enter)
and answers `READ` requests, stashing the caller's Reply until a line completes;
**`shell`** (`servers/shell`) prints the `oxbow$ ` prompt, reads a line, and runs
builtins (`echo`, `help`, unknown → *command not found*). The shell's Console
grant is **revoked at boot**, so it holds zero direct hardware authority and all
output flows through the tty — least privilege enforced by not minting the cap.
The headline works end-to-end at the keyboard: **`oxbow$ echo hi` → `hi`**. See
`docs/abi-v0.md` §11.

**v1 arc 7 — a serial console: complete.** A sixth userspace process
(`servers/serial`) makes COM1 a real *input* device, so you can type at the
shell directly over the serial line — `just run` is fully interactive in your
terminal, BSD-serial-console style, no graphical window needed. It is the §10
IRQ/driver pattern applied to the 16550 UART, with a twist: the device is
**shared with the kernel** (which owns config + the TX path), split by direction
and enforced by capabilities — the driver is granted RBR/LSR as **`R_IN`-only**
I/O-port caps, so it can physically only *read* the UART; a write faults
`E_RIGHTS`. Each received byte is forwarded to the tty as `TAG_TTY_CHAR`, joining
keyboard input in the one line discipline (DEL and 0x08 both rub out). See
`docs/abi-v0.md` §12. (Headless test: COM1 on a TCP socket, driven by
`tools/serial_expect.py`.)

**v1 arc 8 — userspace process spawning: complete.** The boot now drops straight
into a clean `oxbow$ ` prompt (no demo spam); the pong/beta demo is registered as
**spawnable Image capabilities** and launched on demand. `sys_spawn` loads an
Image into a fresh address space and grants it a starter capability set named in
a spawn message — the parent's Memory budget *pays* for the child (the seL4-honest
model), and a Notification is signalled when the child exits (lifecycle without
leaks; process/thread slots are reused). The shell gains `run`: **`run hello`**
spawns a one-line program, **`run pong`** wires an endpoint between two freshly
spawned children (the full PONG regression — IPC, zero-copy shmem, E_GONE, tick,
sys_map — on demand). A program can only launch images it was *granted* (zero
ambient authority; spawn-by-handle). See `docs/abi-v0.md` §13.

**v1 arc 9 — cooked-mode line discipline: complete.** The tty now synchronizes
echo with the reader: keystrokes echo live while the shell waits in `READ`, but
buffer **un-echoed** while the shell is busy (running a command, emitting its
output + prompt) and flush at the next `READ`, after the prompt. So pasting a
whole command — or typing the next one before the previous finishes — no longer
tangles the echo with output; each command's echo lands grouped with its own
prompt, and type-ahead edits (backspace) are invisible. A pure `servers/tty`
change (no ABI/shell/driver change). See `docs/abi-v0.md` §12.5.

### Next — toward a fuller userspace

A **filesystem** (VFS naming server + ramdisk) → coreutils → a **libc/POSIX
shim**. POSIX and the Unix feel live in *userspace* over the capability kernel
(the Redox model) — the kernel stays capability-pure. Plus, eventually: frame
reclamation + budget refund, the orphaned selftest rework, untyped/retype +
`sys_unmap`, SMP, and the `aarch64` port.

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
