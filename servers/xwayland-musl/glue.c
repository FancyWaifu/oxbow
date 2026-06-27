/* oxbow glue for Xwayland: a real (built-in) SHA1 the xserver os layer needs (it
 * otherwise wants OpenSSL), plus link stubs for subsystems we deliberately leave out:
 *  - libXfont2 (server-side core fonts; modern X clients use client-side Xft/freetype)
 *  - libffi static trampolines (wayland's dispatcher uses ffi_call, not closures)
 *  - the drm-lease proxy destroy (drm-lease/GPU leasing is off in this software build)
 * The __oxbow_* runtime shims are NOT here — real oxbow-rt (linked by the crate) provides
 * them. This file holds only what isn't part of oxbow-rt or the standard libs. */
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

/* ---- compact public-domain SHA1 (Steve Reid style) ---- */
typedef struct { uint32_t s[5]; uint32_t c[2]; unsigned char b[64]; } SHA1_CTX;
#define ROL(v, n) (((v) << (n)) | ((v) >> (32 - (n))))
static void sha1_tx(uint32_t st[5], const unsigned char *d)
{
    uint32_t a = st[0], b = st[1], c = st[2], e = st[3], f = st[4], w[80];
    for (int i = 0; i < 16; i++)
        w[i] = __builtin_bswap32(((const uint32_t *)d)[i]);
    for (int i = 16; i < 80; i++)
        w[i] = ROL(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
    for (int i = 0; i < 80; i++) {
        uint32_t x, k;
        if (i < 20) { x = (b & c) | ((~b) & e); k = 0x5A827999; }
        else if (i < 40) { x = b ^ c ^ e; k = 0x6ED9EBA1; }
        else if (i < 60) { x = (b & c) | (b & e) | (c & e); k = 0x8F1BBCDC; }
        else { x = b ^ c ^ e; k = 0xCA62C1D6; }
        uint32_t t = ROL(a, 5) + x + f + k + w[i];
        f = e; e = c; c = ROL(b, 30); b = a; a = t;
    }
    st[0] += a; st[1] += b; st[2] += c; st[3] += e; st[4] += f;
}
void *x_sha1_init(void)
{
    SHA1_CTX *c = malloc(sizeof *c);
    if (!c) return 0;
    c->s[0] = 0x67452301; c->s[1] = 0xEFCDAB89; c->s[2] = 0x98BADCFE;
    c->s[3] = 0x10325476; c->s[4] = 0xC3D2E1F0; c->c[0] = c->c[1] = 0;
    return c;
}
int x_sha1_update(void *ctx, void *data, int n)
{
    SHA1_CTX *c = ctx;
    uint32_t i, j = (c->c[0] >> 3) & 63;
    if ((c->c[0] += n << 3) < (uint32_t)(n << 3)) c->c[1]++;
    c->c[1] += n >> 29;
    if (j + n > 63) {
        memcpy(&c->b[j], data, (i = 64 - j));
        sha1_tx(c->s, c->b);
        for (; i + 63 < (uint32_t)n; i += 64)
            sha1_tx(c->s, (unsigned char *)data + i);
        j = 0;
    } else i = 0;
    memcpy(&c->b[j], (unsigned char *)data + i, n - i);
    return 1;
}
int x_sha1_final(void *ctx, unsigned char out[20])
{
    SHA1_CTX *c = ctx;
    unsigned char fc[8];
    for (int i = 0; i < 8; i++)
        fc[i] = (unsigned char)(c->c[(i >= 4) ? 0 : 1] >> ((3 - (i & 3)) * 8));
    unsigned char one = 0200, z = 0;
    x_sha1_update(c, &one, 1);
    while (((c->c[0] >> 3) & 63) != 56) x_sha1_update(c, &z, 1);
    x_sha1_update(c, fc, 8);
    for (int i = 0; i < 20; i++)
        out[i] = (unsigned char)(c->s[i >> 2] >> ((3 - (i & 3)) * 8));
    free(c);
    return 1;
}

/* ---- libXfont2 stubs (no server-side core fonts) ---- */
int xfont2_init(void *a) { (void)a; return 1; }
int xfont2_init_glyph_caching(void) { return 1; }
int xfont2_parse_glyph_caching_mode(char *s) { (void)s; return 0; }
int xfont2_query_text_extents(void *a, unsigned long b, void *c, void *d) { (void)a;(void)b;(void)c;(void)d; return 0; }
int xfont2_query_glyph_extents(void *a, void *b, unsigned long c, void *d) { (void)a;(void)b;(void)c;(void)d; return 0; }
int xfont2_add_font_names_name(void *a, void *b) { (void)a;(void)b; return 0; }
void xfont2_free_font_names(void *a) { (void)a; }
void *xfont2_make_font_names_record(unsigned long a) { (void)a; return 0; }
void xfont2_free_font_pattern_cache(void *a) { (void)a; }
void *xfont2_make_font_pattern_cache(void) { return 0; }

/* ---- libffi static-trampoline stubs (wayland uses ffi_call, not closures) ---- */
int ffi_tramp_set_parms(void *a, void *b, void *c) { (void)a;(void)b;(void)c; return 0; }
__attribute__((visibility("hidden"))) int ffi_tramp_is_present(void *a) { (void)a; return 0; }

/* ---- drm-lease proxy destroy (drm-lease/GPU leasing disabled) ---- */
void wp_drm_lease_device_v1_destroy(void *p) { (void)p; }
