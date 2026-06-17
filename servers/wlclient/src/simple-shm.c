/* The "rings" demo — now an oxui app (§64). Everything that used to be ~1000
 * lines of Wayland/shm/event-loop boilerplate is gone; this is just the animation
 * (paint_rings) plus four lines of main. The window/buffer/loop plumbing lives in
 * liboxui. This file is the proof that the oxui API is sufficient. */
#include "config.h"
#include <stdint.h>
#include <stdlib.h> /* abs */
#include "oxui.h"

/* The classic weston simple-shm rings, drawn into the oxui canvas. A white border
 * (the old "padding") frames an animated concentric-rings pattern driven by time. */
static void
paint_rings(oxui_window *w, oxui_canvas c, void *user)
{
    (void)w; (void)user;
    const int padding = 20;
    const int width = c.width, height = c.height;
    const uint32_t time = c.time_ms;
    uint32_t *image = c.pixels;

    /* clear to white so the padding frame shows (buffers are reused each frame) */
    for (int i = 0; i < width * height; i++)
        image[i] = 0xffffffff;

    const int halfh = padding + (height - padding * 2) / 2;
    const int halfw = padding + (width  - padding * 2) / 2;
    int ir, or;
    or = (halfw < halfh ? halfw : halfh) - 8;
    ir = or - 32;
    or *= or;
    ir *= ir;

    for (int y = padding; y < height - padding; y++) {
        int y2 = (y - halfh) * (y - halfh);
        for (int x = padding; x < width - padding; x++) {
            uint32_t v;
            int r2 = (x - halfw) * (x - halfw) + y2;
            if (r2 < ir)
                v = (r2 / 32 + time / 64) * 0x0080401;
            else if (r2 < or)
                v = (y + time / 32) * 0x0080401;
            else
                v = (x + time / 16) * 0x0080401;
            v &= 0x00ffffff;
            if (abs(x - y) > 6 && abs(x + y - height) > 6)
                v |= 0xff000000;
            image[y * width + x] = v;
        }
    }
}

int
main(void)
{
    oxui_window *w = oxui_window_create("rings", 256, 256);
    if (!w)
        return 1;
    oxui_handlers h = { .draw = paint_rings, .animate = 1, .extra_fd = -1 };
    oxui_run(w, &h, NULL);
    oxui_window_destroy(w);
    return 0;
}
