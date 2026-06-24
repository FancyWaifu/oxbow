# oxbow

A from-scratch, secure-minimal **capability microkernel** written in Rust — an
OpenBSD-shaped security ethos expressed through an seL4-leaned capability ABI,
with a full Unix-flavored userland built *on top of* the capabilities (the Redox
model: the kernel stays capability-pure, the POSIX feel lives in user space).

It boots on `x86_64` under QEMU **and on real hardware** (a Proxmox KVM VM gets a
real LAN IP and does real TCP to the internet). Arch-specific code is walled
behind `kernel/src/arch/`, so an `aarch64` port stays possible.

> *An OpenBSD-shaped secure microkernel with an seL4-grade capability ABI
> underneath it — and a graphical Unix on top.*

## Design at a glance

| Axis | Choice |
|---|---|
| Kernel | Microkernel — user-mode drivers/servers, IPC-first, SMP |
| IPC | Synchronous call/reply **endpoints** + a flat per-process **handle table** |
| Security | **Zero ambient authority**, W^X always, one attenuation primitive, `pledge`/`mimmutable` |
| Userland | Capability-native; a POSIX/musl personality runs unmodified Unix apps |
| Graphics | Own virtio-gpu driver + a from-scratch Wayland compositor + a GNOME-style shell |
| Boot | Limine (server binaries ride in as modules) |
| License | BSD-2-Clause |

The kernel ABI is specified — normatively — in [`docs/abi-v0.md`](docs/abi-v0.md).
Read that first; in a microkernel, everything is downstream of the IPC/capability ABI.

## What works today

oxbow is a small kernel with a surprisingly large userland on top of it. All of
the following runs:

- **Capability microkernel core** — zero ambient authority (every syscall takes a
  handle), synchronous IPC endpoints with badged capabilities, a per-process
  handle table, user-funded memory (seL4-style "untyped" budgets), W^X
  everywhere, a kernel CSPRNG (RDSEED/RDRAND → ChaCha20), `pledge`, and
  `mimmutable`. **SMP** — real user threads run across multiple cores.

- **A persistent filesystem** — a real **ext2** on a `virtio-blk` disk (vendored
  lwext4), surviving reboots. The coreutils (`ls`/`cat`/`cp`/`mv`/`rm`/`mkdir`/…)
  are separate, **capability-confined** programs living as files in `/bin` — each
  is handed only the directory/file capability it needs, never a global namespace.

- **A network stack, from scratch** — an `e1000` driver, Ethernet/ARP/IPv4/ICMP/
  UDP/TCP, DHCP, and a socket **capability** API. **HTTPS is validated**:
  `curl https://…` works (BearSSL + the kernel CSPRNG), with DNS via a full
  **c-ares** port backing `getaddrinfo` system-wide.

- **Real software** — a **POSIX/musl personality** (a userland Linux-syscall
  translation layer) runs **unmodified upstream Unix programs**: `dash` (a real
  `/bin/sh`), `awk`, the **kilo** editor, **darkhttpd**, and **GNU netcat**. On
  top of an oxbow libc it also runs **Lua, MicroPython, QuickJS, and curl**, plus
  cross-compiled **Rust `std`**. And **TinyCC runs *on* oxbow** — `cc src.c -o
  /bin/prog` compiles a C program on the machine itself.

- **A graphical desktop** — oxbow's **own virtio-gpu driver** (2D scanout, runtime
  modeset, a hardware cursor), a **from-scratch Wayland compositor** (`oxcomp`,
  built on a port of libwayland), and a **GNOME-style shell**: a top bar with a
  live clock, an Activities launcher, draggable/resizable/closable windows, and a
  graphical login greeter. Apps draw real pixels through Wayland. And yes — **it
  runs DOOM.**

- **Security-tested** — a `jail` confinement showcase, an executable
  capability-law spec with a crafted-ELF fuzzer, SMP race proof-of-concepts, and
  a multi-round internal pentest (all findings fixed).

The large C trees in the repo are *vendored* upstream sources (lwext4, BearSSL,
c-ares, libffi, libwayland, FreeType, the language runtimes, the ported Unix
apps) compiled to run on oxbow — see `.gitattributes`. The kernel itself is
from-scratch Rust.

## Quick start

```sh
git clone https://github.com/FancyWaifu/oxbow.git
cd oxbow
just disk     # create the persistent disk image (once)
just play     # build everything, boot a graphical window, log in on screen
```

`just play` opens a window; log in as **`root` / `root`** (the first login asks
you to set a new password). From there you have a shell, a desktop (click
**Activities** to launch apps), networking, and a disk whose files survive
reboots. Prefer a headless terminal-only boot? Use `just run` (kernel + serial
console on your terminal, no window).

> First boot formats and seeds the ext2 disk from the initrd, which takes a few
> tens of seconds — wait for `[fsd] ready` before logging in.

See [Prerequisites](#prerequisites) below to install the toolchain first.

## Prerequisites

You need: a Rust toolchain (via `rustup` — the pinned nightly installs
automatically from `rust-toolchain.toml`), `just`, `qemu-system-x86_64`,
`xorriso`, a `clang`/LLVM toolchain (the vendored C ports compile with it),
`make`, and the **Limine** bootloader (v11).

### Linux (Debian / Ubuntu)

```sh
# 1. Rust (rustup auto-installs the pinned nightly + rust-src/llvm-tools on first build)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"

# 2. Build tools
sudo apt update
sudo apt install -y qemu-system-x86 xorriso clang llvm make git
cargo install just            # or: sudo apt install just  (on 24.04+)

# 3. Limine v11 (prebuilt binaries + the host install tool)
git clone https://github.com/limine-bootloader/limine.git \
  --branch=v11.x-binary --depth=1 ~/oxbow-limine-src
make -C ~/oxbow-limine-src
```

Fedora: `sudo dnf install qemu-system-x86 xorriso clang llvm make git`.
Arch: `sudo pacman -S qemu-base xorriso clang llvm make git just`.

### macOS (Apple Silicon or Intel)

```sh
# 1. Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"

# 2. Command-line clang/make/git, then the rest via Homebrew
xcode-select --install
brew install just qemu xorriso llvm

# 3. Limine v11
git clone https://github.com/limine-bootloader/limine.git \
  --branch=v11.x-binary --depth=1 ~/oxbow-limine-src
make -C ~/oxbow-limine-src
```

The build uses `llvm-strip` (shipped by Rust's `llvm-tools` component, found
automatically), so Apple's `strip` being ELF-blind is a non-issue. If a vendored
C port fails to assemble an archive, put Homebrew LLVM ahead of Apple clang for
that build: `export PATH="$(brew --prefix llvm)/bin:$PATH"`.

### Windows (WSL2)

The build is driven by a Unix shell and tools (`dd`, `tar`, `find`, `xorriso`,
…), so build oxbow inside **WSL2**, not native Windows:

```powershell
wsl --install -d Ubuntu        # in an elevated PowerShell, then reboot
```

Open the Ubuntu shell and follow the **Linux (Debian / Ubuntu)** steps above.
`just run` (headless serial) works as-is. For the graphical `just play` you need
the WSLg GUI support that ships with Windows 11 / recent Windows 10; on older
setups, use `just run` or point `qemu` at a Windows X server.

## Running

```sh
just play          # graphical window + persistent disk + networking (log in root/root)
just run           # headless: kernel + an interactive serial shell on your terminal
just disk          # create the persistent disk image (oxbow-disk.img); run once
just build         # compile just the kernel
just iso           # build the bootable ISO without running it
just gdb           # boot under QEMU stopped, waiting for gdb on :1234
just clean
```

If Limine lives somewhere other than `~/oxbow-limine-src`, point the build at it:
`LIMINE_DIR=/path/to/limine just play`.

The produced `oxbow.iso` is a real hybrid BIOS+UEFI bootable image — it also runs
on a physical machine or a KVM/Proxmox VM (where oxbow brings up a real network).

## Repository layout

```
kernel/    the microkernel (no_std, no_main; boots via Limine), arch/ for x86_64
abi/       oxbow-abi — syscall numbers, rights, errors, MsgBuf (shared kernel+user)
rt/        oxbow-rt  — userland runtime: _start, syscall stubs, libc-ish helpers
libc/      oxbow-libc — a small C library for the native (non-musl) programs
servers/   user-mode servers + apps: drivers (kbd, net, blk, gpu), fs, tty, shell,
           the Wayland compositor (oxcomp) + toolkit (oxui), coreutils, doom, …
userland/  the musl/POSIX personality (the Linux-syscall translation layer)
docs/      abi-v0.md — the normative capability/IPC ABI (read this first)
tools/     build-time helpers (initrd packer, serial test harness, …)
```

## License

BSD-2-Clause. The vendored upstream C sources retain their own licenses.
