/* sysmon — a live system monitor, written as a net-new oxui app (§64).
 *
 * This is the whole program: one draw callback that renders uptime + memory +
 * an activity bar, and four lines of main. No Wayland, shm, xkb, buffers, or
 * event loop — oxui owns all of that; oxui_text owns FreeType. Proof that, with
 * the toolkit in place, a new GUI program is just "draw + main".
 */
#include "config.h"
extern unsigned long ox_uptime_ms(void);                 /* Rust shim */
extern void ox_meminfo(unsigned long *used_kib, unsigned long *total_kib);

#include <stdint.h>
#include <stdio.h>
#include "oxui.h"
#include "oxui_text.h"

static void put2(char *b, unsigned v) { b[0] = '0' + (v / 10) % 10; b[1] = '0' + v % 10; }

static void fill(oxui_canvas c, int x0, int y0, int w, int h, uint32_t col)
{
    for (int y = y0; y < y0 + h; y++) {
        if (y < 0 || y >= c.height) continue;
        for (int x = x0; x < x0 + w; x++) {
            if (x < 0 || x >= c.width) continue;
            c.pixels[y * c.width + x] = col;
        }
    }
}

static void
draw(oxui_window *w, oxui_canvas c, void *user)
{
    (void)w; (void)user;
    /* background */
    fill(c, 0, 0, c.width, c.height, 0x00141c24);

    int lh = oxui_text_line_height();
    int x = 16, y = 12;
    char buf[80];

    oxui_text(c, x, y, "oxbow system monitor", 0x00ffffff);
    y += lh + 8;

    /* uptime HH:MM:SS (manual zero-pad: our printf ignores width) */
    unsigned long ms = ox_uptime_ms();
    unsigned long s = ms / 1000;
    char up[12];
    put2(up, (unsigned)(s / 3600)); up[2] = ':';
    put2(up + 3, (unsigned)((s / 60) % 60)); up[5] = ':';
    put2(up + 6, (unsigned)(s % 60)); up[8] = 0;
    snprintf(buf, sizeof buf, "uptime  %s", up);
    oxui_text(c, x, y, buf, 0x0080d0ff);
    y += lh + 4;

    /* memory used / total */
    unsigned long used = 0, total = 0;
    ox_meminfo(&used, &total);
    snprintf(buf, sizeof buf, "memory  %lu / %lu MiB", used / 1024, total / 1024);
    oxui_text(c, x, y, buf, 0x0080ffa0);
    y += lh + 4;

    /* memory usage bar */
    int bx = 16, bw = c.width - 32, bh = 12;
    fill(c, bx, y, bw, bh, 0x00242c34);
    int frac = total ? (int)((unsigned long long)used * bw / total) : 0;
    fill(c, bx, y, frac, bh, 0x0050a0ff);
    y += bh + 12;

    /* a sweeping activity strip, just to show it's live */
    int sy = y, sh = 8;
    fill(c, bx, sy, bw, sh, 0x00242c34);
    int pos = (int)((ms / 8) % (unsigned long)bw);
    for (int i = 0; i < 28; i++) {
        int px = pos + i;
        if (px < bw) fill(c, bx + px, sy, 1, sh, 0x00a0e0ff);
    }
}

extern void oxui_set_wl_slot(int); /* from liboxui.so (§96 Phase 3) */

int
main(void)
{
    /* §96 Phase 3: sysmon is dynamically linked, so oxcomp hands it BOOT_FS_ROOT at
     * slot 1 (ld-oxbow opens /lib/liboxui.so there) and the Wayland socket at slot 4. */
    oxui_set_wl_slot(4);
    oxui_window *w = oxui_window_create("system monitor", 360, 200);
    if (!w)
        return 1;
    /* §65: refresh ~4x/second so the clock ticks and the strip sweeps, but SLEEP in
     * the kernel between repaints (no busy-poll) — and still wake instantly for input. */
    oxui_handlers h = { .draw = draw, .redraw_interval_ms = 250, .extra_fd = -1 };
    oxui_run(w, &h, NULL);
    oxui_window_destroy(w);
    return 0;
}
