#!/bin/bash
# §96 dyntest: build the dynamic hello-world (libadd.so + dynhello) host-side with
# clang + rust-lld. --hash-style=sysv so ld-oxbow's DT_HASH symbol-count parsing works.
set -e
cd "$(dirname "$0")"
CLANG=/opt/homebrew/opt/llvm/bin/clang
LLD=$(find ~/.rustup -name rust-lld 2>/dev/null | head -1)
CF="--target=x86_64-unknown-none -ffreestanding -fno-stack-protector -nostdinc -nostdlib"
mkdir -p out
$CLANG $CF -fPIC -c add.c -o out/add.o
$LLD -flavor gnu -shared -soname libadd.so --hash-style=sysv -o out/libadd.so out/add.o
$CLANG $CF -c dynhello.c -o out/dynhello.o
$LLD -flavor gnu -o out/dynhello out/dynhello.o -T dyn.ld \
    -dynamic-linker /lib/ld-oxbow -z now --hash-style=sysv -L out -ladd
echo "built out/libadd.so + out/dynhello"
