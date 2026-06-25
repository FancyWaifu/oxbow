#!/bin/bash
# §96 Phase 3: build oxui (oxui.c + oxui_text.c) as a PIC shared library, liboxui.so,
# so its consumer apps (sysmon, oxterm, wlclient, doom) don't recompile when oxui
# changes. The .so leaves ALL of libc / libwayland / libffi / libxkbcommon / FreeType
# UNDEFINED — those stay static-in-exe in each consumer and are EXPORTED back to the
# .so via --dynamic-list (servers/oxui/oxui-exports.list). ld-oxbow resolves the .so's
# JUMP_SLOT/GLOB_DAT against the exe (exe-first) at runtime. No R_X86_64_COPY (verified).
set -e
cd "$(dirname "$0")/../.."   # repo root
RES="$(clang -print-resource-dir)/include"
CLANG=/opt/homebrew/opt/llvm/bin/clang
[ -x "$CLANG" ] || CLANG=clang
LLD=$(find ~/.rustup -name rust-lld 2>/dev/null | head -1)
CF="--target=x86_64-unknown-none -nostdinc -isystem $RES -I libc/include -ffreestanding -fno-stack-protector -fno-builtin -Wno-everything -O2 -fPIC"
# Same include set as sysmon/build.rs's main unit (wayland + ffi + xkb + ft + oxui).
INC="-I servers/oxterm/include -I servers/oxwl/wl-include -I servers/oxffi/ffi-include -I servers/oxxkb/xkb/include -I servers/oxft/ft/include -I servers/oxui/include -I servers/oxterm/font -I servers/oxwl -DHAVE_CONFIG_H"
OUT=servers/oxui/out
mkdir -p "$OUT"
$CLANG $CF $INC -c servers/oxui/oxui.c      -o "$OUT/oxui.o"
$CLANG $CF $INC -c servers/oxui/oxui_text.c -o "$OUT/oxui_text.o"
# -shared allows the UNDEF symbols (resolved at runtime from the exe). sysv hash so
# ld-oxbow's DT_HASH nchain symbol-count parse works.
$LLD -flavor gnu -shared -soname liboxui.so --hash-style=sysv \
    -o "$OUT/liboxui.so" "$OUT/oxui.o" "$OUT/oxui_text.o"
echo "built $OUT/liboxui.so ($(stat -f%z "$OUT/liboxui.so" 2>/dev/null || stat -c%s "$OUT/liboxui.so") bytes)"
