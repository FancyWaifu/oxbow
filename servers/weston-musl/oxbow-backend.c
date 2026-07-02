/* oxbow-backend.c — a minimal libweston backend for oxbow. Modeled on Weston's
 * backend-headless (pixman renderer, zero udev/libinput/launcher/dlopen), but the
 * output's pixman image wraps oxbow's linear framebuffer (FB_MMIO) DIRECTLY, so the
 * pixman renderer draws straight into the buffer the gpu scans out — render == present.
 *
 * The framebuffer pointer + geometry come from oxbow_fb_* globals, filled by the native
 * Rust shim (which maps BOOT_GPU_FB) before oxbow_backend_create() runs. Input is injected
 * separately (P3); there is no libinput/udev here. See docs/weston-port.md. */
#include "config.h"

#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <stdbool.h>
#include <drm_fourcc.h>

#include <libweston/libweston.h>
#include "shared/helpers.h"
#include "pixman-renderer.h"
#include "presentation-time-server-protocol.h"
#include <libweston/windowed-output-api.h>

/* Filled by the Rust shim (oxbow_map_fb) before the backend is created. */
uint32_t *oxbow_fb = NULL;
int oxbow_fb_w = 1280;
int oxbow_fb_h = 800;
int oxbow_fb_stride_bytes = 1280 * 4;

/* ---- software cursor (weston tracks the pointer but nothing paints it) ---- */
extern double oxbow_ptr_x, oxbow_ptr_y; /* current pointer position (set by input) */

#define CUR_W 11
#define CUR_H 17
static const char *cursor_rows[CUR_H] = {
    "X",          "XX",         "X.X",        "X..X",       "X...X",
    "X....X",     "X.....X",    "X......X",   "X.......X",  "X........X",
    "X.....XXXXX","X..X..X",    "X.X X..X",   "XX  X..X",   "X    X..X",
    "     X..X",  "      XX",
};
static uint32_t cur_save[CUR_W * CUR_H];
static int cur_sx = -1, cur_sy = -1; /* where the saved-under pixels belong */

static void cursor_restore(void)
{
    if (cur_sx < 0 || !oxbow_fb)
        return;
    for (int j = 0; j < CUR_H; j++) {
        int y = cur_sy + j;
        if (y < 0 || y >= oxbow_fb_h) continue;
        for (int i = 0; i < CUR_W; i++) {
            int x = cur_sx + i;
            if (x < 0 || x >= oxbow_fb_w) continue;
            oxbow_fb[y * oxbow_fb_w + x] = cur_save[j * CUR_W + i];
        }
    }
    cur_sx = -1;
}

static void cursor_draw(int px, int py)
{
    if (!oxbow_fb)
        return;
    for (int j = 0; j < CUR_H; j++)
        for (int i = 0; i < CUR_W; i++) {
            int x = px + i, y = py + j;
            if (x >= 0 && x < oxbow_fb_w && y >= 0 && y < oxbow_fb_h)
                cur_save[j * CUR_W + i] = oxbow_fb[y * oxbow_fb_w + x];
        }
    cur_sx = px;
    cur_sy = py;
    for (int j = 0; j < CUR_H; j++) {
        int y = py + j;
        if (y < 0 || y >= oxbow_fb_h) continue;
        const char *row = cursor_rows[j];
        for (int i = 0; i < CUR_W && row[i]; i++) {
            int x = px + i;
            if (x < 0 || x >= oxbow_fb_w) continue;
            if (row[i] == 'X') oxbow_fb[y * oxbow_fb_w + x] = 0xFF000000;
            else if (row[i] == '.') oxbow_fb[y * oxbow_fb_w + x] = 0xFFFFFFFF;
        }
    }
}

/* Move the software cursor with a direct framebuffer blit: restore the pixels under the
 * old position, then save+draw at the new one. Called from the input handler on pointer
 * motion so the cursor tracks WITHOUT scheduling a full compositor repaint (a software
 * pixman composite of every surface) just to move an 11x17 sprite — the main lag source. */
void oxbow_cursor_move(int px, int py)
{
    if (!oxbow_fb)
        return;
    cursor_restore();
    cursor_draw(px, py);
}

struct oxbow_backend {
    struct weston_backend base;
    struct weston_compositor *compositor;
};

struct oxbow_head {
    struct weston_head base;
};

struct oxbow_output {
    struct weston_output base;
    struct weston_mode mode;
    struct wl_event_source *finish_frame_timer;
    pixman_image_t *image; /* wraps FB_MMIO — no backing malloc */
};

static inline struct oxbow_output *to_oxbow_output(struct weston_output *base)
{
    return container_of(base, struct oxbow_output, base);
}
static inline struct oxbow_backend *to_oxbow_backend(struct weston_compositor *base)
{
    return container_of(base->backend, struct oxbow_backend, base);
}

static int oxbow_output_start_repaint_loop(struct weston_output *output)
{
    struct timespec ts;
    weston_compositor_read_presentation_clock(output->compositor, &ts);
    weston_output_finish_frame(output, &ts, WP_PRESENTATION_FEEDBACK_INVALID);
    return 0;
}

static int finish_frame_handler(void *data)
{
    struct oxbow_output *output = data;
    struct timespec ts;
    weston_compositor_read_presentation_clock(output->base.compositor, &ts);
    weston_output_finish_frame(&output->base, &ts, 0);
    return 1;
}

static int oxbow_output_repaint(struct weston_output *output_base,
                                pixman_region32_t *damage, void *repaint_data)
{
    struct oxbow_output *output = to_oxbow_output(output_base);
    struct weston_compositor *ec = output->base.compositor;

    /* Paint the desktop background color ONCE (0xFF1B2A4A dark navy). Weston composites
     * only DAMAGED regions each frame, so clearing every frame would wipe non-animating
     * windows (they'd never be re-composited). Clearing once lets weston draw windows on
     * top and keep them — a static window persists in the fb between its own redraws.
     * (A proper background is normally a shell surface; this is the lightweight stand-in.) */
    static int cleared;
    if (oxbow_fb && !cleared) {
        cleared = 1;
        int npix = oxbow_fb_w * oxbow_fb_h;
        for (int i = 0; i < npix; i++)
            oxbow_fb[i] = 0xFF1B2A4A;
    }

    /* Remove last frame's software cursor before weston composites (weston doesn't know
     * about it), then re-composite, then repaint the cursor on top at the live position. */
    cursor_restore();

    ec->renderer->repaint_output(&output->base, damage);

    cursor_draw((int)oxbow_ptr_x, (int)oxbow_ptr_y);

    pixman_region32_subtract(&ec->primary_plane.damage,
                             &ec->primary_plane.damage, damage);

    /* ~60 fps repaint tick (same cadence as headless). */
    wl_event_source_timer_update(output->finish_frame_timer, 16);
    return 0;
}

static int oxbow_output_enable(struct weston_output *base)
{
    struct oxbow_output *output = to_oxbow_output(base);
    struct oxbow_backend *b = to_oxbow_backend(base->compositor);
    struct wl_event_loop *loop;
    /* use_shadow=true: pixman keeps a shadow buffer and copies only DAMAGED regions to the
     * fb each frame, instead of repainting the whole 1080p buffer in software every frame.
     * Big perf win — full-frame software composites were the main source of sluggishness. */
    const struct pixman_renderer_output_options options = { .use_shadow = true };

    /* Wrap oxbow's framebuffer as the render target — render == present. */
    output->image = pixman_image_create_bits(PIXMAN_x8r8g8b8, oxbow_fb_w, oxbow_fb_h,
                                             oxbow_fb, oxbow_fb_stride_bytes);
    if (!output->image)
        return -1;

    if (pixman_renderer_output_create(&output->base, &options) < 0) {
        pixman_image_unref(output->image);
        return -1;
    }
    pixman_renderer_output_set_buffer(&output->base, output->image);

    loop = wl_display_get_event_loop(b->compositor->wl_display);
    output->finish_frame_timer = wl_event_loop_add_timer(loop, finish_frame_handler, output);
    return 0;
}

static int oxbow_output_disable(struct weston_output *base)
{
    struct oxbow_output *output = to_oxbow_output(base);
    if (!base->enabled)
        return 0;
    wl_event_source_remove(output->finish_frame_timer);
    pixman_renderer_output_destroy(&output->base);
    pixman_image_unref(output->image);
    return 0;
}

static void oxbow_output_destroy(struct weston_output *base)
{
    struct oxbow_output *output = to_oxbow_output(base);
    oxbow_output_disable(&output->base);
    weston_output_release(&output->base);
    free(output);
}

/* Called by the frontend via the windowed-output API to set the mode/size. */
static int oxbow_output_set_size(struct weston_output *base, int width, int height)
{
    struct oxbow_output *output = to_oxbow_output(base);
    struct weston_head *head;

    assert(!output->base.current_mode);
    assert(output->base.scale);

    wl_list_for_each(head, &output->base.head_list, output_link) {
        weston_head_set_monitor_strings(head, "oxbow", "weston-fb", NULL);
        weston_head_set_physical_size(head, width, height);
    }

    output->mode.flags = WL_OUTPUT_MODE_CURRENT | WL_OUTPUT_MODE_PREFERRED;
    output->mode.width = width * output->base.scale;
    output->mode.height = height * output->base.scale;
    output->mode.refresh = 60000;
    wl_list_insert(&output->base.mode_list, &output->mode.link);
    output->base.current_mode = &output->mode;

    output->base.start_repaint_loop = oxbow_output_start_repaint_loop;
    output->base.repaint = oxbow_output_repaint;
    output->base.assign_planes = NULL;
    output->base.set_backlight = NULL;
    output->base.set_dpms = NULL;
    output->base.switch_mode = NULL;
    return 0;
}

static struct weston_output *oxbow_output_create(struct weston_compositor *compositor,
                                                 const char *name)
{
    struct oxbow_output *output;
    assert(name);
    output = zalloc(sizeof *output);
    if (!output)
        return NULL;
    weston_output_init(&output->base, compositor, name);
    output->base.destroy = oxbow_output_destroy;
    output->base.disable = oxbow_output_disable;
    output->base.enable = oxbow_output_enable;
    output->base.attach_head = NULL;
    weston_compositor_add_pending_output(&output->base, compositor);
    return &output->base;
}

static int oxbow_head_create(struct weston_compositor *compositor, const char *name)
{
    struct oxbow_head *head;
    assert(name);
    head = zalloc(sizeof *head);
    if (!head)
        return -1;
    weston_head_init(&head->base, name);
    weston_head_set_connection_status(&head->base, true);
    weston_compositor_add_head(compositor, &head->base);
    return 0;
}

static void oxbow_destroy(struct weston_compositor *ec)
{
    struct oxbow_backend *b = to_oxbow_backend(ec);
    weston_compositor_shutdown(ec);
    free(b);
}

static const struct weston_windowed_output_api api = {
    oxbow_output_set_size,
    oxbow_head_create,
};

/* Called directly from oxbow-main.c (no dlopen / weston_backend_init ABI). */
struct weston_backend *oxbow_backend_create(struct weston_compositor *compositor)
{
    struct oxbow_backend *b;

    b = zalloc(sizeof *b);
    if (!b)
        return NULL;
    b->compositor = compositor;
    compositor->backend = &b->base;

    if (weston_compositor_set_presentation_clock_software(compositor) < 0)
        goto err;

    b->base.destroy = oxbow_destroy;
    b->base.create_output = oxbow_output_create;

    if (pixman_renderer_init(compositor) < 0)
        goto err;

    if (weston_plugin_api_register(compositor, WESTON_WINDOWED_OUTPUT_API_NAME,
                                   &api, sizeof(api)) < 0) {
        weston_log("oxbow: failed to register windowed-output API\n");
        goto err;
    }
    return &b->base;

err:
    free(b);
    return NULL;
}
