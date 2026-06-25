/* §96 Phase 3: libffi's closure path (ffi_prep_closure_loc in ffi64.c/ffiw64.c)
 * references ffi_tramp_is_present from tramp.c, which the oxbow libffi port omits.
 * A static exe GC's the unused closure code; a -shared .so exports/retains ALL its
 * globals, dragging that code in. wayland uses ffi_call (forward calls), not closures,
 * so this path is dead on oxbow — stub the predicate to "no static trampoline". */
int ffi_tramp_is_present(void *closure) {
    (void)closure;
    return 0;
}
