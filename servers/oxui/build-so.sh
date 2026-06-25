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
CF="--target=x86_64-unknown-none -nostdinc -isystem $RES -I libc/include -ffreestanding -fno-stack-protector -fno-builtin -Wno-everything -O2 -fPIC -ffunction-sections -fdata-sections"
# Same include set as sysmon/build.rs's main unit (wayland + ffi + xkb + ft + oxui).
INC="-I servers/oxterm/include -I servers/oxwl/wl-include -I servers/oxffi/ffi-include -I servers/oxxkb/xkb/include -I servers/oxft/ft/include -I servers/oxui/include -I servers/oxterm/font -I servers/oxwl -DHAVE_CONFIG_H"
OUT=servers/oxui/out
mkdir -p "$OUT"
OBJS=""
# oxui itself + its text/font + local mem funcs (memcpy/memset/... — see mem.c).
for f in oxui oxui_text mem; do
    $CLANG $CF $INC -c "servers/oxui/$f.c" -o "$OUT/$f.o"; OBJS="$OBJS $OUT/$f.o"
done
# §96 Phase 3: BUNDLE wayland + libffi INTO the .so. A non-PIE exe can't export DATA
# symbols (wayland's wl_*_interface tables) to a DSO, so wayland — which both defines
# AND uses those tables — must live in the .so, not the exe. wayland's wl_proxy_marshal
# dispatch uses libffi, so libffi comes too (incl. its hand-written PIC asm trampolines).
# xkb/freetype/libc stay static-in-exe (oxui imports only their FUNCTIONS = JUMP_SLOT).
for f in wayland-util connection wayland-os wayland-protocol xdg-shell-protocol wayland-client; do
    $CLANG $CF $INC -c "servers/oxwl/wl-src/$f.c" -o "$OUT/wl_$f.o"; OBJS="$OBJS $OUT/wl_$f.o"
done
for f in prep_cif types raw_api x86/ffi64 x86/ffiw64; do
    n=$(basename "$f")
    $CLANG $CF $INC -c "servers/oxffi/ffi-src/$f.c" -o "$OUT/ffi_$n.o"; OBJS="$OBJS $OUT/ffi_$n.o"
done
for f in unix64 win64; do
    $CLANG $CF $INC -c "servers/oxffi/ffi-src/x86/$f.S" -o "$OUT/ffi_$f.o"; OBJS="$OBJS $OUT/ffi_$f.o"
done
# ffi_tramp_is_present: the oxbow libffi port omits tramp.c (static-trampoline closures);
# a -shared .so retains the unused closure code that references it. Stub it (see ffi_stub.c).
$CLANG $CF $INC -c servers/oxui/ffi_stub.c -o "$OUT/ffi_stub.o"; OBJS="$OBJS $OUT/ffi_stub.o"
# -shared allows the UNDEF symbols (resolved at runtime from the exe). sysv hash so
# ld-oxbow's DT_HASH nchain symbol-count parse works.
# --version-script exports only oxui_* (hides wayland/ffi internals); --gc-sections then
# drops the unreachable bundled code (and its libc/socket deps). sysv hash for ld-oxbow.
$LLD -flavor gnu -shared -soname liboxui.so --hash-style=sysv \
    --version-script servers/oxui/oxui.ver --gc-sections \
    -o "$OUT/liboxui.so" $OBJS
echo "built $OUT/liboxui.so ($(stat -f%z "$OUT/liboxui.so" 2>/dev/null || stat -c%s "$OUT/liboxui.so") bytes)"
