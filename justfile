# oxbow build runner.
# Override the Limine path with:  LIMINE_DIR=/path/to/limine just run
LIMINE_DIR := env_var_or_default("LIMINE_DIR", home_directory() / "oxbow-limine-src")
KERNEL     := "target/x86_64-unknown-none/debug/oxbow-kernel"
ISO        := "oxbow.iso"

# QEMU flags shared by `run` and `gdb`. q35 machine, serial routed to stdio,
# no display, the isa-debug-exit device so a future test harness can exit QEMU
# from inside the kernel, and a legacy virtio-blk disk (oxbow-disk.img) for
# persistent storage. Create the disk once with:  just disk
qemu_flags := "-M q35 -m 256M -smp 4 -cdrom " + ISO + " -boot d -serial stdio -display none -no-reboot -no-shutdown -device isa-debug-exit,iobase=0xf4,iosize=0x04 -netdev user,id=net0 -device e1000,netdev=net0 -drive file=oxbow-disk.img,if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0 -device virtio-gpu-pci"

default: run

# Compile just the kernel for the bare-metal target.
build:
    cargo build -p oxbow-kernel

# Compile the user-mode servers. Their own RUSTFLAGS REPLACE the kernel's config
# rustflags (dropping code-model=kernel); build-std + target still apply. The
# user link layout comes from each crate's build.rs, so it can't leak here.
build-server:
    RUSTFLAGS="-C relocation-model=static" cargo build -p pong
    RUSTFLAGS="-C relocation-model=static" cargo build -p beta
    RUSTFLAGS="-C relocation-model=static" cargo build -p kbd
    RUSTFLAGS="-C relocation-model=static" cargo build -p tty
    # SSE on: the shell embeds Lua 5.4, whose C does double arithmetic (// % ^ /).
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p shell
    RUSTFLAGS="-C relocation-model=static" cargo build -p serial
    RUSTFLAGS="-C relocation-model=static" cargo build -p hello
    RUSTFLAGS="-C relocation-model=static" cargo build -p ld-oxbow
    RUSTFLAGS="-C relocation-model=static" cargo build -p thrtest
    RUSTFLAGS="-C relocation-model=static" cargo build -p bench
    RUSTFLAGS="-C relocation-model=static" cargo build -p badge
    RUSTFLAGS="-C relocation-model=static" cargo build -p net
    RUSTFLAGS="-C relocation-model=static" cargo build -p blk
    RUSTFLAGS="-C relocation-model=static" cargo build -p gpu
    RUSTFLAGS="-C relocation-model=static" cargo build -p fb
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxcomp
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p wlclient
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxterm
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p sysmon
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p doom
    cargo build -p havoc-musl  # real upstream Wayland terminal (musl); flags come from its build.rs
    cargo build -p xwayland-musl  # real X server (xorg-server Wayland DDX) as an oxcomp client
    cargo build -p xclient-musl  # minimal raw-X11 client (loopback TCP -> Xwayland)
    cargo build -p xcbdemo-musl  # X client on real libxcb (loopback TCP -> Xwayland)
    cargo build -p xlibdemo-musl  # X client on real libX11/Xlib (over libxcb)
    cargo build -p xeyes-musl  # first unmodified upstream X app (Xt/Xext/Xmu toolkit chain)
    cargo build -p twm-musl  # the X window manager (decorates X clients in Xwayland)
    cargo build -p xterm-musl  # real upstream terminal X client (core fonts, forkpty -> /bin/sh)
    RUSTFLAGS="-C relocation-model=static" cargo build -p fsd
    RUSTFLAGS="-C relocation-model=static" cargo build -p cat
    RUSTFLAGS="-C relocation-model=static" cargo build -p ls
    RUSTFLAGS="-C relocation-model=static" cargo build -p mkdir
    RUSTFLAGS="-C relocation-model=static" cargo build -p touch
    RUSTFLAGS="-C relocation-model=static" cargo build -p rm
    RUSTFLAGS="-C relocation-model=static" cargo build -p mv
    RUSTFLAGS="-C relocation-model=static" cargo build -p cp
    RUSTFLAGS="-C relocation-model=static" cargo build -p jail
    RUSTFLAGS="-C relocation-model=static" cargo build -p fsext
    # drift speaks DRIFT's crypto (X25519/ChaCha20-Poly1305) — SIMD that needs
    # hardware SSE. Build it with soft-float off + SSE on (the kernel enabled the
    # FPU + does per-thread FXSAVE), and the non-SIMD curve25519 backend.
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2 --cfg curve25519_dalek_backend="serial"' cargo build -p drift
    RUSTFLAGS="-C relocation-model=static" cargo build -p cc-hello
    # The float-ABI flags (-soft-float,+sse,+sse2) matter for any tool that passes
    # a double across the C->Rust libc boundary (seq's printf("%f"), printf, od);
    # the kernel enabled SSE at boot, and clang-compiled C passes doubles in XMM, so
    # oxbow-libc must use the same hardware-SSE float ABI. Harmless for the rest.
    for t in wc head tail find grep true false yes seq basename dirname tee rev cut cmp paste fold comm uniq tr strings od printf split sleep sort nl tac cksum md5sum sha1sum sha256sum sha512sum sha224sum sha384sum sha512-224sum sha512-256sum; do \
        RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p $t; \
    done
    RUSTFLAGS="-C relocation-model=static" cargo build -p ps
    RUSTFLAGS="-C relocation-model=static" cargo build -p kill
    RUSTFLAGS="-C relocation-model=static" cargo build -p spin
    RUSTFLAGS="-C relocation-model=static" cargo build -p oxtcc
    # Lua uses doubles heavily; its clang-compiled C passes floats in XMM
    # (hardware SSE), so oxbow-libc must too — build with soft-float OFF + SSE ON
    # (the kernel enabled SSE at boot) so the float ABI matches across the
    # Rust↔C boundary (pow/floor args, printf %f varargs).
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxlua
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxpy
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxqjs
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxcurl
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxares
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxffi
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxxkb
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxvterm
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxft
    RUSTFLAGS='-C relocation-model=static -C target-feature=-soft-float,+sse,+sse2' cargo build -p oxwl

# Same, but with the ABI negative-path selftests compiled in.
build-server-selftest:
    RUSTFLAGS="-C relocation-model=static" cargo build -p pong --features selftest

# Same, but pong faults at startup (fault-containment test).
build-server-faulttest:
    RUSTFLAGS="-C relocation-model=static" cargo build -p pong --features faulttest
    RUSTFLAGS="-C relocation-model=static" cargo build -p beta

# Both servers with the isolation demo (hex @0x200000; beta then suicides).
build-server-isolation:
    RUSTFLAGS="-C relocation-model=static" cargo build -p pong --features isolation
    RUSTFLAGS="-C relocation-model=static" cargo build -p beta --features isolation

# Assemble a hybrid BIOS+UEFI bootable ISO from whatever binaries currently
# exist (kernel + the pong build packed as the server module / v0 initrd).
_iso:
    rm -rf iso_root
    mkdir -p iso_root/boot/limine iso_root/EFI/BOOT
    cp {{KERNEL}} iso_root/boot/oxbow
    cp target/x86_64-unknown-none/debug/pong iso_root/boot/server.elf
    cp target/x86_64-unknown-none/debug/beta iso_root/boot/beta.elf
    cp target/x86_64-unknown-none/debug/kbd iso_root/boot/kbd.elf
    cp target/x86_64-unknown-none/debug/tty iso_root/boot/tty.elf
    cp target/x86_64-unknown-none/debug/shell iso_root/boot/shell.elf
    cp target/x86_64-unknown-none/debug/serial iso_root/boot/serial.elf
    cp target/x86_64-unknown-none/debug/hello iso_root/boot/hello.elf
    cp target/x86_64-unknown-none/debug/ld-oxbow iso_root/boot/ld-oxbow.elf
    cp target/x86_64-unknown-none/debug/badge iso_root/boot/badge.elf
    # §95: the Rust `std` demo as a boot module (kernel spawns it → output to the
    # serial console), if prebuilt. Proves real Rust std runs on oxbow.
    cp target/x86_64-unknown-none/debug/fsd iso_root/boot/fs.elf
    -strip -S iso_root/boot/fs.elf
    cp target/x86_64-unknown-none/debug/net iso_root/boot/net.elf
    cp target/x86_64-unknown-none/debug/blk iso_root/boot/blk.elf
    cp target/x86_64-unknown-none/debug/gpu iso_root/boot/gpu.elf
    -strip -S iso_root/boot/gpu.elf
    cp target/x86_64-unknown-none/debug/wlclient iso_root/boot/wlclient.elf
    -strip -S iso_root/boot/wlclient.elf
    cp target/x86_64-unknown-none/debug/oxterm iso_root/boot/oxterm.elf
    -strip -S iso_root/boot/oxterm.elf
    cp target/x86_64-unknown-none/debug/sysmon iso_root/boot/sysmon.elf
    -strip -S iso_root/boot/sysmon.elf
    cp target/x86_64-unknown-none/debug/doom iso_root/boot/doom.elf
    -strip -S iso_root/boot/doom.elf
    cp target/x86_64-unknown-none/debug/havoc iso_root/boot/havoc.elf
    -strip -S iso_root/boot/havoc.elf
    cp target/x86_64-unknown-none/debug/Xwayland iso_root/boot/xwayland.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/xwayland.elf
    cp target/x86_64-unknown-none/debug/xclient iso_root/boot/xclient.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/xclient.elf
    cp target/x86_64-unknown-none/debug/xcbdemo iso_root/boot/xcbdemo.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/xcbdemo.elf
    cp target/x86_64-unknown-none/debug/xlibdemo iso_root/boot/xlibdemo.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/xlibdemo.elf
    cp target/x86_64-unknown-none/debug/xeyes iso_root/boot/xeyes.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/xeyes.elf
    cp target/x86_64-unknown-none/debug/twm iso_root/boot/twm.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/twm.elf
    cp target/x86_64-unknown-none/debug/xterm iso_root/boot/xterm.elf
    "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/xterm.elf
    cargo build -p weston-musl  # real upstream Weston — the DEFAULT compositor (P6)
    # P6: Weston is now the default compositor. It's spawned as the "oxcomp" boot module so
    # it inherits BOOT_GPU_FB + the input grants, and does window management via
    # libweston-desktop. The old hand-written oxcomp is retired from the boot; set OXCOMP=1
    # to roll back to it (it's still built for comparison/fallback).
    if [ -n "${OXCOMP:-}" ]; then \
      cp target/x86_64-unknown-none/debug/oxcomp iso_root/boot/oxcomp.elf; \
      strip -S iso_root/boot/oxcomp.elf || true; \
      echo "[iso] OXCOMP=1 -> booting the legacy oxcomp compositor"; \
    else \
      cp target/x86_64-unknown-none/debug/weston iso_root/boot/oxcomp.elf; \
      "$(find $(rustc --print sysroot) -name llvm-strip | head -1)" --strip-all iso_root/boot/oxcomp.elf; \
      echo "[iso] booting Weston as the compositor (default; OXCOMP=1 for the legacy one)"; \
    fi
    cp target/x86_64-unknown-none/debug/cat iso_root/boot/cat.elf
    cp target/x86_64-unknown-none/debug/ls iso_root/boot/ls.elf
    cp target/x86_64-unknown-none/debug/mkdir iso_root/boot/mkdir.elf
    cp target/x86_64-unknown-none/debug/touch iso_root/boot/touch.elf
    cp target/x86_64-unknown-none/debug/rm iso_root/boot/rm.elf
    cp target/x86_64-unknown-none/debug/mv iso_root/boot/mv.elf
    cp target/x86_64-unknown-none/debug/cp iso_root/boot/cp.elf
    cp target/x86_64-unknown-none/debug/jail iso_root/boot/jail.elf
    cp target/x86_64-unknown-none/debug/fstest iso_root/boot/fstest.elf
    -strip -S iso_root/boot/fstest.elf
    cp target/x86_64-unknown-none/debug/drift iso_root/boot/drift.elf
    cp target/x86_64-unknown-none/debug/cc-hello iso_root/boot/cc-hello.elf
    cp target/x86_64-unknown-none/debug/tcc iso_root/boot/tcc.elf
    -strip -S iso_root/boot/tcc.elf
    cp target/x86_64-unknown-none/debug/lua iso_root/boot/lua.elf
    -strip -S iso_root/boot/lua.elf
    cp target/x86_64-unknown-none/debug/micropython iso_root/boot/micropython.elf
    -strip -S iso_root/boot/micropython.elf
    cp target/x86_64-unknown-none/debug/qjs iso_root/boot/qjs.elf
    -strip -S iso_root/boot/qjs.elf
    cp target/x86_64-unknown-none/debug/curl iso_root/boot/curl.elf
    -strip -S iso_root/boot/curl.elf
    cp target/x86_64-unknown-none/debug/cares-test iso_root/boot/cares-test.elf
    -strip -S iso_root/boot/cares-test.elf
    cp target/x86_64-unknown-none/debug/ffi-test iso_root/boot/ffi-test.elf
    -strip -S iso_root/boot/ffi-test.elf
    cp target/x86_64-unknown-none/debug/wl-test iso_root/boot/wl-test.elf
    -strip -S iso_root/boot/wl-test.elf
    cp target/x86_64-unknown-none/debug/xkb-test iso_root/boot/xkb-test.elf
    -strip -S iso_root/boot/xkb-test.elf
    cp target/x86_64-unknown-none/debug/vterm-test iso_root/boot/vterm-test.elf
    -strip -S iso_root/boot/vterm-test.elf
    cp target/x86_64-unknown-none/debug/ft-test iso_root/boot/ft-test.elf
    -strip -S iso_root/boot/ft-test.elf
    # Stage the filesystem: the FHS skeleton (servers/fs/initrd) plus a small
    # on-device source browse under /usr/src/oxbow.
    # §94: the fs is a 256-NODE ramfs. Copying the whole kernel+servers source
    # (~160 nodes) exhausted the node table, so /bin's programs couldn't be indexed
    # and bare commands hit "command not found". Ship just the design docs +
    # manifests as the "source on device" gesture (the full tree lives in git); a
    # bigger/writable fs is the next arc.
    rm -rf build/initrd
    mkdir -p build/initrd build/initrd/usr/src/oxbow
    cp -R servers/fs/initrd/. build/initrd/
    cp -R docs build/initrd/usr/src/oxbow/
    cp Cargo.toml justfile limine.conf build/initrd/usr/src/oxbow/ 2>/dev/null || true
    # exec-from-fs demo (§33): a STRIPPED copy of `hello` placed on the fs at
    # /bin/hello, so `exec /bin/hello` loads + runs an ELF from disk. Stripping
    # (with llvm-strip — Apple strip can't touch ELF) shrinks 3.4 MB -> ~115 KB so
    # the shell's 56-byte FS_READ loop slurps it quickly.
    mkdir -p build/initrd/bin
    # §94: the coreutils ship as FILES in /bin, not baked-in boot images — so a
    # user can add/delete programs and `make their own thing`. The shell resolves
    # bare command names here (PATH), reachable by every logged-in user. Stripped
    # (llvm-strip; Apple strip can't touch ELF) so the 56-byte FS_READ loop is quick.
    STRIP=$(find $(rustc --print sysroot) -name llvm-strip | head -1); \
    for t in hello ls cat mkdir touch rm mv cp thrtest bench wc head tail find grep true false yes seq basename dirname tee rev cut cmp paste fold comm uniq tr strings od printf split sleep sort nl tac cksum md5sum sha1sum sha256sum sha512sum sha224sum sha384sum sha512-224sum sha512-256sum muslhello awk kilo darkhttpd netcat sh ps kill spin; do \
      cp target/x86_64-unknown-none/debug/$t build/initrd/bin/$t; \
      "$STRIP" --strip-all build/initrd/bin/$t; \
    done
    # §95: the Rust `std` demo — a cross-compiled std program (Vec/String/println!)
    # built for x86_64-unknown-oxbow, driven by oxbow-rt's hosted shims. Proves real
    # Rust std runs on oxbow. Copied only if prebuilt (needs the patched-std fork —
    # see std-port/). Already stripped at build time.
    [ -f std-port/oxhello-demo.elf ] && cp std-port/oxhello-demo.elf build/initrd/bin/oxhello || true
    # UDP MTU echo test (netmap Stage 2): a std program that round-trips 400/800/1400-byte
    # datagrams through the per-socket zero-copy frame. Copied only if prebuilt.
    [ -f std-port/apps/udpmtu/target/x86_64-unknown-oxbow/release/udpmtu ] && { cp std-port/apps/udpmtu/target/x86_64-unknown-oxbow/release/udpmtu build/initrd/bin/udpmtu; STRIP=$(find $(rustc --print sysroot) -name llvm-strip | head -1); "$STRIP" --strip-all build/initrd/bin/udpmtu; } || true
    # Self-hosting (§35): liboxbow_libc.a staged at /lib/c.a — the C library
    # archive tcc statically links to produce a standalone binary on oxbow.
    # `cc src.c -o out` expands to `tcc -static src.c -o out /lib/c.a`. Built with
    # the same static relocation model as the servers (direct relocs, no PIC GOT
    # that tcc would mishandle). Short path /lib/c.a fits the 55-byte spawn argv.
    RUSTFLAGS="-C relocation-model=static" cargo build -p oxbow-libc --release
    mkdir -p build/initrd/lib
    cp target/x86_64-unknown-none/release/liboxbow_libc.a build/initrd/lib/c.a
    # §96 dyntest: the dynamic-linking hello-world. /lib/libadd.so + /bin/dynhello,
    # copied only if prebuilt (servers/dyntest/build.sh). dynhello is a dynamically
    # linked ELF (PT_INTERP=/lib/ld-oxbow, DT_NEEDED libadd.so) — exec'ing it runs
    # ld-oxbow, which links libadd.so and prints "ld-oxbow OK: 3+4=7".
    [ -f servers/dyntest/out/libadd.so ] && cp servers/dyntest/out/libadd.so build/initrd/lib/libadd.so || true
    [ -f servers/dyntest/out/dynhello ] && cp servers/dyntest/out/dynhello build/initrd/bin/dynhello || true
    # §96 Phase 2: single-runtime symbol scope. /lib/libacc.so + /bin/dyntwo. dyntwo
    # imports accumulate() from libacc.so; libacc.so calls BACK into dyntwo's exported
    # exe_add() (--dynamic-list). exit 15 proves the .so->exe callback resolves exe-first
    # and both share one runtime state (the mechanism a shared liboxui needs in Phase 3).
    [ -f servers/dyntest/out/libacc.so ] && cp servers/dyntest/out/libacc.so build/initrd/lib/libacc.so || true
    [ -f servers/dyntest/out/dyntwo ] && cp servers/dyntest/out/dyntwo build/initrd/bin/dyntwo || true
    # §96 Phase 3: oxui as a shared library. /lib/liboxui.so (built by
    # servers/oxui/build-so.sh) — its consumers (sysmon etc., dynamically linked) get
    # the .so resolved by ld-oxbow at runtime, so an oxui change rebuilds only the .so.
    [ -f servers/oxui/out/liboxui.so ] && cp servers/oxui/out/liboxui.so build/initrd/lib/liboxui.so || true
    # /usr/include (§36): oxbow-libc headers (stdio.h, string.h, …) at
    # /usr/include + tcc's own builtin headers (stdarg.h, stddef.h, …) at
    # /usr/lib/tcc/include. tcc's default sysinclude path is "{B}/include:
    # /usr/include" with B=/usr/lib/tcc, so on-device `#include <stdio.h>` resolves.
    mkdir -p build/initrd/usr/include build/initrd/usr/lib/tcc/include
    cp -R libc/include/. build/initrd/usr/include/
    cp servers/tcc/tinycc/include/*.h build/initrd/usr/lib/tcc/include/
    # Drop build artifacts + the (self-referential) initrd skeleton copy.
    find build/initrd/usr/src/oxbow -type d -name target -prune -exec rm -rf {} + 2>/dev/null || true
    rm -rf build/initrd/usr/src/oxbow/servers/fs/initrd
    COPYFILE_DISABLE=1 tar --format=ustar -cf iso_root/boot/initrd.tar -C build/initrd .
    cp limine.conf iso_root/boot/limine/
    cp {{LIMINE_DIR}}/limine-bios.sys {{LIMINE_DIR}}/limine-bios-cd.bin {{LIMINE_DIR}}/limine-uefi-cd.bin iso_root/boot/limine/
    cp {{LIMINE_DIR}}/BOOTX64.EFI {{LIMINE_DIR}}/BOOTIA32.EFI iso_root/EFI/BOOT/
    xorriso -as mkisofs -R -r -J \
        -b boot/limine/limine-bios-cd.bin \
        -no-emul-boot -boot-load-size 4 -boot-info-table \
        --efi-boot boot/limine/limine-uefi-cd.bin \
        -efi-boot-part --efi-boot-image --protective-msdos-label \
        iso_root -o {{ISO}}
    {{LIMINE_DIR}}/limine bios-install {{ISO}}

# Build everything and assemble the ISO (normal server).
iso: build build-server _iso

# Build the ISO and boot it. Ctrl-A then X to quit QEMU.
run: iso
    qemu-system-x86_64 {{qemu_flags}}

# Boot the selftest build: runs the ABI negative-path tests, then the PONG trace.
run-selftest: build build-server-selftest _iso
    qemu-system-x86_64 {{qemu_flags}}

# Boot a build where pong faults at startup — proves the machine survives it.
run-faulttest: build build-server-faulttest _iso
    qemu-system-x86_64 {{qemu_flags}}

# Boot the isolation demo: same vaddr / different bytes, hostile beta dies alone.
run-isolation: build build-server-isolation _iso
    qemu-system-x86_64 {{qemu_flags}}

# Boot WITH a graphical window so you can log in on screen. The oxterm terminal
# (FreeType + libvterm) shows the shell; click the QEMU window to give it keyboard
# focus, then log in (bryson/bryson or root/root2). Kernel output + a serial mirror
# stream to this terminal. 512M for the graphics stack. (Quit: close the window,
# or Ctrl-A X in this terminal.)
run-tty: iso
    qemu-system-x86_64 -M q35 -m 512M -smp 4 -cdrom {{ISO}} -boot d -serial stdio -display cocoa -no-reboot -no-shutdown -device isa-debug-exit,iobase=0xf4,iosize=0x04 -drive file=oxbow-disk.img,if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0

# `just play` — hop into oxbow for hands-on use: a graphical window, your PERSISTENT
# disk (files survive reboots), and networking. Log in ON SCREEN as root/root2 (Tab
# between the username/password fields, Enter submits); CLICK the window first to give
# it keyboard/mouse focus (Ctrl-Alt-G releases). Kernel + serial logs stream to this
# terminal — and you also get an interactive serial shell here once it boots.
# IMPORTANT: no `-device virtio-gpu-pci` — that forces the GPU's hardware cursor, which
# QEMU does not render under Mac (TCG) emulation (so the mouse vanishes) and is slower.
# Plain std VGA gives Limine a linear framebuffer; oxcomp composites into it and draws a
# SOFTWARE cursor that's visible + lighter under emulation. (On real hw / KVM, either
# works; the GPU path is only worth it there.)
play: iso
    qemu-system-x86_64 -M q35 -m 512M -smp 4 -cdrom {{ISO}} -boot d -serial stdio -display cocoa -no-reboot -no-shutdown -drive file=oxbow-disk.img,if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0 -netdev user,id=net0 -device e1000,netdev=net0

# Headless serial-console test target: COM1 routed to a TCP socket so a harness
# can both TYPE (write) and READ on one stream. server=on,wait=on makes QEMU
# block at startup until the harness connects, so no boot output is lost.
# (By design this hangs until a client connects — that's the point.)
run-serial-tcp PORT="45454": iso
    qemu-system-x86_64 -M q35 -m 256M -cdrom {{ISO}} -boot d -serial tcp:127.0.0.1:{{PORT}},server=on,wait=on -display none -no-reboot -no-shutdown -device isa-debug-exit,iobase=0xf4,iosize=0x04 -netdev user,id=net0 -device e1000,netdev=net0

# Boot stopped, waiting for a debugger:  (in another shell) gdb -ex 'target remote :1234'
gdb: iso
    qemu-system-x86_64 {{qemu_flags}} -S -s

clean:
    cargo clean
    rm -rf iso_root {{ISO}}

# Create the persistent-storage disk image (256 MiB raw) if it does not exist.
# The first boot seeds it from the ~10 MiB initrd via ext2; 16 MiB was too tight
# (4 KiB-block rounding on many small files could exhaust it mid-seed), so give the
# seed generous headroom + room for user files. Recreate after a size change:
#   rm oxbow-disk.img && just disk
disk:
    [ -f oxbow-disk.img ] || dd if=/dev/zero of=oxbow-disk.img bs=1m count=256
