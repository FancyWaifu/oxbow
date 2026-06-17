/* oxui_text — FreeType text rendering for oxui (§64). Lazily initializes a
 * FreeType face from the embedded DejaVu Mono font and blits strings into a
 * canvas with grayscale-coverage alpha blending. */
#include "config.h"
#include <stdint.h>
#include <ft2build.h>
#include FT_FREETYPE_H
#include "dejavu_mono.h"
#include "oxui_text.h"

static FT_Library s_lib;
static FT_Face    s_face;
static int        s_cell_h, s_baseline, s_advance;
static int        s_ready;

static void ensure_face(void)
{
    if (s_ready)
        return;
    s_ready = 1; /* mark attempted; on failure draws nothing */
    if (FT_Init_FreeType(&s_lib))
        return;
    if (FT_New_Memory_Face(s_lib, dejavu_mono_ttf, (FT_Long)dejavu_mono_ttf_len, 0, &s_face))
        return;
    FT_Set_Pixel_Sizes(s_face, 0, 16);
    s_cell_h  = (int)(s_face->size->metrics.height >> 6);
    s_baseline = (int)(s_face->size->metrics.ascender >> 6);
    s_advance = (int)(s_face->size->metrics.max_advance >> 6);
    if (s_cell_h < 1) s_cell_h = 18;
    if (s_advance < 1) s_advance = 9;
}

static void blit_glyph(uint32_t *buf, int bw, int bh, FT_Bitmap *bmp,
                       int px, int py, uint32_t fg)
{
    for (unsigned gy = 0; gy < bmp->rows; gy++) {
        int y = py + (int)gy;
        if (y < 0 || y >= bh) continue;
        for (unsigned gx = 0; gx < bmp->width; gx++) {
            int x = px + (int)gx;
            if (x < 0 || x >= bw) continue;
            unsigned a = bmp->buffer[gy * bmp->pitch + gx];
            if (!a) continue;
            uint32_t bg = buf[y * bw + x];
            uint32_t r = (((fg >> 16) & 0xff) * a + ((bg >> 16) & 0xff) * (255 - a)) / 255;
            uint32_t g = (((fg >> 8) & 0xff) * a + ((bg >> 8) & 0xff) * (255 - a)) / 255;
            uint32_t b = ((fg & 0xff) * a + (bg & 0xff) * (255 - a)) / 255;
            buf[y * bw + x] = (r << 16) | (g << 8) | b;
        }
    }
}

int oxui_text(oxui_canvas c, int x, int y, const char *str, uint32_t color)
{
    ensure_face();
    if (!s_face)
        return x;
    int x0 = x;
    for (const char *p = str; *p; p++) {
        if (*p == '\n') { x = x0; y += s_cell_h; continue; }
        if (FT_Load_Char(s_face, (unsigned char)*p, FT_LOAD_RENDER)) {
            x += s_advance;
            continue;
        }
        FT_GlyphSlot g = s_face->glyph;
        blit_glyph(c.pixels, c.width, c.height, &g->bitmap,
                   x + g->bitmap_left, y + s_baseline - g->bitmap_top, color);
        x += s_advance;
    }
    return x;
}

int oxui_text_line_height(void)
{
    ensure_face();
    return s_cell_h ? s_cell_h : 18;
}
