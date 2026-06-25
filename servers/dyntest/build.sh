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

# §96 Phase 2: single-runtime symbol scope. acc.so calls BACK into the exe
# (exe_add), which the exe EXPORTS via --dynamic-list. Proves exe-first resolution
# + shared state. acc.so leaves exe_add UNDEF (allowed for -shared); dyntwo links
# acc and forces exe_add into its .dynsym so the .so's import resolves against it.
$CLANG $CF -fPIC -c acc.c -o out/acc.o
$LLD -flavor gnu -shared -soname libacc.so --hash-style=sysv -o out/libacc.so out/acc.o
$CLANG $CF -c dyntwo.c -o out/dyntwo.o
$LLD -flavor gnu -o out/dyntwo out/dyntwo.o -T dyn.ld \
    -dynamic-linker /lib/ld-oxbow -z now --hash-style=sysv \
    --dynamic-list dyntwo.dynlist -L out -lacc
echo "built out/libacc.so + out/dyntwo"
