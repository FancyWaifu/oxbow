/* Hand-written fficonfig.h for oxbow (x86_64 SysV) — the subset of the
 * autoconf-generated config libffi's portable + x86_64 sources actually use.
 * Closures (executable trampolines) are NOT used by our consumer (libwayland
 * only calls ffi_call), so the exec-memory machinery is left minimal. */
#ifndef FFICONFIG_H
#define FFICONFIG_H

#define STDC_HEADERS 1
#define HAVE_ALLOCA 1
#define HAVE_ALLOCA_H 1
#define HAVE_MEMCPY 1

/* x86_64 sizes (LP64). */
#define SIZEOF_SIZE_T 8
#define SIZEOF_DOUBLE 8
#define SIZEOF_LONG_DOUBLE 16
#define HAVE_LONG_DOUBLE 1
#define HAVE_LONG_DOUBLE_VARIANT 0

/* Assembler capabilities — clang's integrated assembler supports these. */
#define HAVE_AS_CFI_PSEUDO_OP 1
#define HAVE_AS_X86_64_UNWIND_SECTION_TYPE 1
/* PCREL(X) := X - . (clang wants this, not the GNU `X@rel` variant). */
#define HAVE_AS_X86_PCREL 1
#define EH_FRAME_FLAGS "aw"

/* Closure / trampoline strategy: no iOS-style trampoline table, no static
 * trampolines (we don't use closures). */
#define FFI_EXEC_TRAMPOLINE_TABLE 0
#define FFI_EXEC_STATIC_TRAMP 0

/* In assembly (LIBFFI_ASM, set by the .S files before they include us),
 * FFI_HIDDEN(name) emits the hidden-visibility directive; in C it is the
 * attribute. ffi_common.h includes us and then uses FFI_HIDDEN, so it must be
 * defined here. */
#ifdef LIBFFI_ASM
#define FFI_HIDDEN(name) .hidden name
#else
#define FFI_HIDDEN __attribute__ ((visibility ("hidden")))
#endif

#endif /* FFICONFIG_H */
