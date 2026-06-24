/* oxterm — a Wayland terminal, now an oxui app (§52, §64).
 *
 * Everything that was window/buffer/event-loop/xkb/xdg boilerplate (and the §63
 * fixes we spent a session debugging) now lives in liboxui. What remains here is
 * ONLY the terminal: the libvterm screen model, FreeType glyph rendering, and the
 * shell-output (tty mirror) fd. The app gives oxui three callbacks — paint the
 * grid, drain the tty, (default) close — and oxui does the rest.
 */
#include "config.h"
extern int ox_chan_fd(unsigned int); /* oxbow: inherited fds (1 = Wayland, 20 = tty) */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>

#include <vterm.h>
#include <ft2build.h>
#include FT_FREETYPE_H
#include "dejavu_mono.h"
#include "oxui.h"

/* Terminal state (one window). */
static FT_Library  ft_lib;
static FT_Face     ft_face;
static int         cell_w, cell_h, baseline;
static VTerm      *vt;
static VTermScreen *vts;
static int         g_tty_fd = -1;

/* Alpha-blend one FreeType glyph (grayscale coverage) as fg over bg. */
static void
blit_glyph(uint32_t *buf, int bw, int bh, FT_Bitmap *bmp, int px, int py,
           uint32_t fg, uint32_t bg)
{
    for (unsigned gy = 0; gy < bmp->rows; gy++) {
        int y = py + (int)gy;
        if (y < 0 || y >= bh)
            continue;
        for (unsigned gx = 0; gx < bmp->width; gx++) {
            int x = px + (int)gx;
            if (x < 0 || x >= bw)
                continue;
            unsigned a = bmp->buffer[gy * bmp->pitch + gx];
            if (!a)
                continue;
            uint32_t r = (((fg >> 16) & 0xff) * a + ((bg >> 16) & 0xff) * (255 - a)) / 255;
            uint32_t g = (((fg >> 8) & 0xff) * a + ((bg >> 8) & 0xff) * (255 - a)) / 255;
            uint32_t b = ((fg & 0xff) * a + (bg & 0xff) * (255 - a)) / 255;
            buf[y * bw + x] = (r << 16) | (g << 8) | b;
        }
    }
}

/* Render the libvterm screen grid into the oxui canvas with FreeType. */
static void
render_grid(uint32_t *buf, int bw, int bh)
{
    uint32_t bg = 0x001a1a1a, fg = 0x00d0d0d0;
    for (int i = 0; i < bw * bh; i++)
        buf[i] = bg;
    if (!vt || !ft_face)
        return;
    int cols = bw / cell_w, rows = bh / cell_h;
    for (int r = 0; r < rows; r++) {
        for (int c = 0; c < cols; c++) {
            VTermPos pos = { .row = r, .col = c };
            VTermScreenCell cell;
            if (!vterm_screen_get_cell(vts, pos, &cell))
                continue;
            uint32_t ch = cell.chars[0];
            if (ch == 0 || ch == ' ')
                continue;
            if (FT_Load_Char(ft_face, ch, FT_LOAD_RENDER))
                continue;
            FT_GlyphSlot g = ft_face->glyph;
            int px = c * cell_w + g->bitmap_left;
            int py = r * cell_h + baseline - g->bitmap_top;
            blit_glyph(buf, bw, bh, &g->bitmap, px, py, fg, bg);
        }
    }
}

/* FreeType from the embedded font + libvterm sized to the window grid. */
static void
term_init(int win_w, int win_h)
{
    if (FT_Init_FreeType(&ft_lib))
        return;
    if (FT_New_Memory_Face(ft_lib, dejavu_mono_ttf,
                           (FT_Long)dejavu_mono_ttf_len, 0, &ft_face))
        return;
    FT_Set_Pixel_Sizes(ft_face, 0, 16);
    cell_w = (int)(ft_face->size->metrics.max_advance >> 6);
    cell_h = (int)(ft_face->size->metrics.height >> 6);
    baseline = (int)(ft_face->size->metrics.ascender >> 6);
    if (cell_w < 1) cell_w = 9;
    if (cell_h < 1) cell_h = 18;
    int cols = win_w / cell_w, rows = win_h / cell_h;
    if (cols < 1) cols = 1;
    if (rows < 1) rows = 1;
    vt = vterm_new(rows, cols);
    vterm_set_utf8(vt, 1);
    vts = vterm_obtain_screen(vt);
    vterm_screen_reset(vts, 1);
    /* §53: the shell's output arrives over the tty-mirror channel (spawn slot 20).
     * Non-blocking so draining it never stalls. */
    g_tty_fd = ox_chan_fd(20);
    if (g_tty_fd >= 0)
        fcntl(g_tty_fd, F_SETFL, O_NONBLOCK);
}

/* oxui draw callback: paint the current terminal grid. */
static void
on_draw(oxui_window *w, oxui_canvas c, void *user)
{
    (void)w; (void)user;
    render_grid(c.pixels, c.width, c.height);
}

/* §93b: the compositor resized the window (maximize/tile/drag) → reflow the vterm
 * grid to the new pixel size so the terminal gains/loses rows+cols and renders
 * sharp at the new resolution (instead of the compositor up-scaling a fixed grid).
 * render_grid uses the same cols=bw/cell_w math, so they stay in lock-step. */
static void
on_resize(oxui_window *w, int width, int height, void *user)
{
    (void)w; (void)user;
    if (!vt || cell_w < 1 || cell_h < 1)
        return;
    int cols = width / cell_w, rows = height / cell_h;
    if (cols < 1) cols = 1;
    if (rows < 1) rows = 1;
    vterm_set_size(vt, rows, cols);
}

/* oxui fd_ready callback: new shell output on the tty mirror → feed libvterm and
 * ask oxui to repaint. ONLCR: the console stream uses bare '\n'; libvterm needs
 * '\r\n' or the cursor staircases. */
static void
on_tty(oxui_window *w, void *user)
{
    (void)user;
    if (g_tty_fd < 0 || !vt)
        return;
    char tbuf[1024];
    long n;
    int got = 0;
    while ((n = read(g_tty_fd, tbuf, sizeof tbuf)) > 0) {
        char out[2100];
        int  oi = 0;
        for (long i = 0; i < n; i++) {
            if (tbuf[i] == '\n')
                out[oi++] = '\r';
            out[oi++] = tbuf[i];
        }
        vterm_input_write(vt, out, (size_t)oi);
        got = 1;
    }
    if (got)
        oxui_request_redraw(w);
}

int
main(void)
{
    oxui_window *w = oxui_window_create("oxterm", 720, 400);
    if (!w)
        return 1;
    term_init(oxui_width(w), oxui_height(w));

    oxui_handlers h = {
        .draw     = on_draw,
        .resize   = on_resize, /* §93b: reflow the grid on maximize/tile/resize */
        .extra_fd = g_tty_fd, /* also wait on the shell-output mirror */
        .fd_ready = on_tty,
        .animate  = 0,        /* a terminal is event-driven, not animated */
    };
    oxui_run(w, &h, NULL);
    oxui_window_destroy(w);
    return 0;
}
