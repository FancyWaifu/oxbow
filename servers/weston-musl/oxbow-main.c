/* oxbow-main.c — the minimal Weston frontend for oxbow. Replaces the huge upstream
 * compositor/main.c: no config files, no CLI, no dlopen'd backends/shell. It maps the
 * framebuffer (via the native Rust shim), creates a libweston compositor with the oxbow
 * backend, brings up one output at the fb resolution, and runs the event loop. The
 * heads-changed handshake mirrors main.c's simple_head_enable. Clients (P4) + a shell
 * (P5) come later. See docs/weston-port.md. */
#include "config.h"

#include <stdint.h>
#include <stdlib.h>
#include <stdarg.h>

#include <wayland-server-core.h>
#include <libweston/libweston.h>
#include "shared/helpers.h"
#include <libweston/windowed-output-api.h>

/* framebuffer, filled by oxbow_map_fb + consumed by the backend */
extern uint32_t *oxbow_fb;
extern int oxbow_fb_w, oxbow_fb_h, oxbow_fb_stride_bytes;
extern struct weston_backend *oxbow_backend_create(struct weston_compositor *compositor);
extern void oxbow_input_init(struct weston_compositor *compositor); /* P3: kbd + mouse seat */
extern void oxbow_shell_init(struct weston_compositor *compositor); /* P4/P5: xdg-shell */
extern int oxbow_spawn_wl_client(int app_id);                      /* P4/P5: spawn a client */

/* native shim (oxbow-rt): maps BOOT_GPU_FB at FB_MMIO; returns the ptr + geometry. */
extern uint32_t *oxbow_map_fb(int *w, int *h, int *stride_bytes);

extern long write(int, const void *, unsigned long);
extern int vsnprintf(char *, unsigned long, const char *, va_list);
static void logmsg(const char *s) { unsigned long n = 0; while (s[n]) n++; write(2, s, n); }

/* libweston's default log handler abort()s on first use, so a real one MUST be installed
 * before any weston_log(). Route weston's logs to stderr (→ oxbow console). */
static int ox_vlog(const char *fmt, va_list ap)
{
    char buf[512];
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    if (n < 0) return 0;
    unsigned long len = ((unsigned long)n < sizeof buf) ? (unsigned long)n : sizeof buf - 1;
    write(2, buf, len);
    return n;
}
extern void weston_log_set_handler(int (*log)(const char *, va_list),
                                   int (*cont)(const char *, va_list));

static struct wl_listener heads_changed_listener;

/* When the backend adds a head, create + configure + enable an output for it. */
static void heads_changed(struct wl_listener *listener, void *arg)
{
    struct weston_compositor *compositor = arg;
    const struct weston_windowed_output_api *api =
        weston_windowed_output_get_api(compositor);
    struct weston_head *head = NULL;

    while ((head = weston_compositor_iterate_heads(compositor, head))) {
        if (weston_head_is_connected(head) && !weston_head_is_enabled(head) &&
            !weston_head_is_non_desktop(head)) {
            struct weston_output *output =
                weston_compositor_create_output_with_head(compositor, head);
            if (!output) {
                logmsg("[weston] create_output_with_head failed\n");
            } else {
                weston_output_set_scale(output, 1);
                weston_output_set_transform(output, WL_OUTPUT_TRANSFORM_NORMAL);
                if (api->output_set_size(output, oxbow_fb_w, oxbow_fb_h) < 0) {
                    logmsg("[weston] output_set_size failed\n");
                    weston_output_destroy(output);
                } else if (weston_output_enable(output) < 0) {
                    logmsg("[weston] output_enable failed\n");
                    weston_output_destroy(output);
                } else {
                    logmsg("[weston] output ENABLED on the oxbow framebuffer\n");
                }
            }
        }
        weston_head_reset_device_changed(head);
    }
}

int main(void)
{
    struct wl_display *display;
    struct weston_log_context *log_ctx;
    struct weston_compositor *compositor;
    const struct weston_windowed_output_api *api;

    logmsg("[weston] starting on oxbow\n");
    weston_log_set_handler(ox_vlog, ox_vlog); /* MUST precede any weston_log() (else abort) */

    oxbow_fb = oxbow_map_fb(&oxbow_fb_w, &oxbow_fb_h, &oxbow_fb_stride_bytes);
    if (!oxbow_fb) { logmsg("[weston] framebuffer map failed\n"); return 1; }

    display = wl_display_create();
    if (!display) { logmsg("[weston] wl_display_create failed\n"); return 1; }

    log_ctx = weston_log_ctx_create();
    if (!log_ctx) { logmsg("[weston] weston_log_ctx_create failed\n"); return 1; }

    compositor = weston_compositor_create(display, log_ctx, NULL);
    if (!compositor) { logmsg("[weston] weston_compositor_create failed\n"); return 1; }
    logmsg("[weston] compositor created\n");

    heads_changed_listener.notify = heads_changed;
    weston_compositor_add_heads_changed_listener(compositor, &heads_changed_listener);

    if (!oxbow_backend_create(compositor)) {
        logmsg("[weston] oxbow_backend_create failed\n"); return 1;
    }
    logmsg("[weston] backend created\n");

    oxbow_input_init(compositor); /* P3: keyboard + mouse → weston_seat */
    oxbow_shell_init(compositor); /* P4/P5: xdg-shell so clients can map windows */

    /* Ask the backend (via the windowed API it registered) for a head → heads_changed
     * fires (now or on the first idle) → the output is enabled + starts repainting. */
    api = weston_windowed_output_get_api(compositor);
    if (!api || api->create_head(compositor, "oxbow") < 0) {
        logmsg("[weston] create_head failed\n"); return 1;
    }
    logmsg("[weston] head created\n");

    weston_compositor_wake(compositor);

    /* P5: spawn a Wayland client (inherited-fd model, like oxcomp). Default to an IDLE
     * client (havoc, a real terminal) — NOT the rings demo, which animates every frame and
     * keeps the compositor busy (the main source of perceived lag). An idle desktop lets
     * weston sleep between events (§blocking-wait) so input stays snappy. */
    int apps[] = { 1 /*havoc terminal*/ };
    for (unsigned a = 0; a < sizeof apps / sizeof apps[0]; a++) {
        int cfd = oxbow_spawn_wl_client(apps[a]);
        if (cfd >= 0 && wl_client_create(display, cfd))
            logmsg("[weston] client spawned + attached\n");
        else
            logmsg("[weston] client spawn/attach failed\n");
    }

    logmsg("[weston] entering the event loop\n");
    wl_display_run(display);

    logmsg("[weston] event loop returned; shutting down\n");
    weston_compositor_destroy(compositor);
    weston_log_ctx_destroy(log_ctx);
    wl_display_destroy(display);
    return 0;
}
