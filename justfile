# oxbow build runner.
# Override the Limine path with:  LIMINE_DIR=/path/to/limine just run
LIMINE_DIR := env_var_or_default("LIMINE_DIR", home_directory() / "oxbow-limine-src")
KERNEL     := "target/x86_64-unknown-none/debug/oxbow-kernel"
ISO        := "oxbow.iso"

# QEMU flags shared by `run` and `gdb`. q35 machine, serial routed to stdio,
# no display, and the isa-debug-exit device so a future test harness can exit
# QEMU from inside the kernel.
qemu_flags := "-M q35 -m 256M -cdrom " + ISO + " -boot d -serial stdio -display none -no-reboot -no-shutdown -device isa-debug-exit,iobase=0xf4,iosize=0x04"

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

# Boot stopped, waiting for a debugger:  (in another shell) gdb -ex 'target remote :1234'
gdb: iso
    qemu-system-x86_64 {{qemu_flags}} -S -s

clean:
    cargo clean
    rm -rf iso_root {{ISO}}
