#!/bin/sh
# Build musl libc for oxbow with the personality syscall override (Phase 1 entry).
#
# musl source lives OUT of the repo (vendored, like the Rust std fork). Fetch once:
#   mkdir -p ~/musl-oxbow && cd ~/musl-oxbow
#   curl -LO https://musl.libc.org/releases/musl-1.2.5.tar.gz && tar xzf musl-1.2.5.tar.gz
#
# Then run this script. It drops our syscall_arch.h over musl's (so every syscall
# routes through __oxbow_syscall instead of the `syscall` instruction) and builds a
# static libc.a. Link a program as:
#   musl libc.a  +  oxbow_syscall.o  +  oxbow-rt (feature "hosted")  +  crt
set -eu

MUSL="${MUSL:-$HOME/musl-oxbow/musl-1.2.5}"
PERS="$(cd "$(dirname "$0")" && pwd)"
TARGET=x86_64-unknown-none

[ -d "$MUSL" ] || { echo "musl tree not found at $MUSL (see header comment)"; exit 1; }

# 1. Install the oxbow overrides:
#    - syscall_arch.h: routes musl's C __syscallN through __oxbow_syscall.
#    - __set_thread_area.s: upstream's x86_64 version issues a RAW arch_prctl syscall
#      that bypasses the C override, so installing musl's thread pointer was dropped by
#      the oxbow kernel (fs stayed on the kernel's bare TLS block, locale=NULL → the
#      first setlocale faulted). Ours issues oxbow's SYS_SET_FSBASE directly.
cp "$PERS/syscall_arch.h" "$MUSL/arch/x86_64/syscall_arch.h"
cp "$PERS/__set_thread_area.s" "$MUSL/src/thread/x86_64/__set_thread_area.s"

# 2. Configure + build static musl with clang cross-targeting bare x86_64.
cd "$MUSL"
CFLAGS="--target=$TARGET -ffreestanding -fno-stack-protector -fno-builtin"
./configure --target=x86_64 --disable-shared CC=clang CFLAGS="$CFLAGS" || {
	echo "configure failed — musl's build wants a GNU-ish env; iterate here in Phase 1."
	exit 1
}
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

# 3. Compile the personality dispatcher against our headers.
RES="$(clang -print-resource-dir)/include"
clang --target=$TARGET -ffreestanding -nostdinc -isystem "$RES" -I "$PERS" \
	-c "$PERS/oxbow_syscall.c" -o "$PERS/oxbow_syscall.o"

echo "musl: $MUSL/lib/libc.a"
echo "personality: $PERS/oxbow_syscall.o"
echo "next: link a test program + oxbow-rt[hosted] crt, pack into /bin, boot."
