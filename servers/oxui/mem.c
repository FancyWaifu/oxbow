/* §96 Phase 3: memcpy/memset/memmove/strlen for liboxui.so. These are normally
 * imported from the exe's libc, but oxbow-libc gets them from compiler-builtins-mem
 * with HIDDEN visibility, so they can't be exported across the exe->.so boundary
 * (lld: "non-exported symbol 'memcpy' referenced by DSO"). They're pure + stateless,
 * so the .so carries its own copies (no shared-state concern, unlike malloc/free which
 * stay imported so heap allocations are shared). clang emits calls to these even with
 * -ffreestanding (struct copies, array init), so they must resolve somewhere. */
#include <stddef.h>

void *memcpy(void *d, const void *s, size_t n) {
    unsigned char *dp = d;
    const unsigned char *sp = s;
    while (n--) *dp++ = *sp++;
    return d;
}

void *memset(void *d, int c, size_t n) {
    unsigned char *dp = d;
    while (n--) *dp++ = (unsigned char)c;
    return d;
}

void *memmove(void *d, const void *s, size_t n) {
    unsigned char *dp = d;
    const unsigned char *sp = s;
    if (dp < sp) {
        while (n--) *dp++ = *sp++;
    } else {
        dp += n;
        sp += n;
        while (n--) *--dp = *--sp;
    }
    return d;
}

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) p++;
    return (size_t)(p - s);
}
