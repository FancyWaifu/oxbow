/* oxui — implementation (§64). All the Wayland/shm/xkb/event-loop boilerplate that
 * every GUI client used to copy, written once. Patterns (deferred redraw, dirty
 * flag, event-driven loop, single-memfd-friendly buffer reuse) are the §63 fixes
 * lifted out of oxterm so apps get them for free. */
#include "config.h"
extern int ox_chan_fd(unsigned int);      /* oxbow: inherited Wayland socket (slot 1) */

/* Which inherited capability slot carries the Wayland socket. Default 1 (the usual
 * compositor-spawned-app convention). An app that needs slot 1 for something else — DOOM
 * keeps its filesystem cap there so the WAD opens via stdio — sets this to a free slot
 * BEFORE calling oxui_window_create. */
int oxui_wl_slot = 1;

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <unistd.h>
#include <poll.h>
#include <fcntl.h>
#include <sys/mman.h>

#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include "shared/os-compatibility.h"
#include <libweston/zalloc.h>
#include "xdg-shell-client-protocol.h"
#include "oxui.h"

#define OXUI_NBUF 2 /* double-buffered; sized so both fit a client's mem budget */

struct oxui_buffer {
    struct wl_buffer *buffer;
    uint32_t *pixels;
    int busy;
    int width, height;
    size_t size;
    struct wl_list link;
};

struct oxui_window {
    struct wl_display    *display;
    struct wl_registry   *registry;
    struct wl_compositor *compositor;
    struct wl_shm        *shm;
    struct xdg_wm_base   *wm_base;
    struct wl_seat       *seat;
    struct wl_keyboard   *keyboard;
    struct wl_pointer    *pointer;
    struct wl_surface    *surface;
    struct xdg_surface   *xdg_surface;
    struct xdg_toplevel  *xdg_toplevel;
    struct xkb_context   *xkb_ctx;
    struct xkb_keymap    *xkb_keymap;
    struct xkb_state     *xkb_state;

    struct wl_list buffer_list;
    int  width, height;
    int  wait_for_configure;
    int  dirty;       /* needs a repaint (content changed / resized / first frame) */
    int  running;
    uint32_t time_ms; /* last frame-callback timestamp, for animation */

    const oxui_handlers *h;
    void *user;
};

/* ---- shm buffer pool (the §63-correct version: reuse, defer, never abort) ---- */

static struct oxui_buffer *pick_free_buffer(struct oxui_window *w)
{
    struct oxui_buffer *b, *fresh = NULL, *ready = NULL;
    wl_list_for_each(b, &w->buffer_list, link) {
        if (b->busy)
            continue;
        if (b->buffer && b->width == w->width && b->height == w->height) {
            ready = b; /* a free buffer at the right size, already created — reuse it */
            break;
        }
        if (!fresh)
            fresh = b; /* a slot we can (re)create at the current size */
    }
    return ready ? ready : fresh;
}

static void buffer_release(void *data, struct wl_buffer *buffer)
{
    struct oxui_buffer *b = data;
    (void)buffer;
    b->busy = 0;
}
static const struct wl_buffer_listener buffer_listener = { buffer_release };

static int create_shm(struct oxui_window *w, struct oxui_buffer *b)
{
    int stride = w->width * 4;
    int size = stride * w->height;
    int fd = os_create_anonymous_file(size);
    if (fd < 0)
        return -1;
    void *data = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (data == MAP_FAILED) {
        close(fd);
        return -1;
    }
    /* §93b-fix: the new allocation succeeded — only NOW drop any old buffer in this
     * slot. Allocating first means an OOM above (e.g. a full-screen buffer that
     * doesn't fit the client's memory budget) leaves the old buffer intact to fall
     * back on, instead of destroying it and wedging the client with nothing to draw. */
    if (b->buffer)
        wl_buffer_destroy(b->buffer);
    if (b->pixels)
        munmap(b->pixels, b->size);
    struct wl_shm_pool *pool = wl_shm_create_pool(w->shm, fd, size);
    b->buffer = wl_shm_pool_create_buffer(pool, 0, w->width, w->height, stride,
                                          WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(b->buffer, &buffer_listener, b);
    wl_shm_pool_destroy(pool);
    close(fd);
    b->pixels = data;
    b->size = size;
    b->width = w->width;
    b->height = w->height;
    return 0;
}

/* Get a buffer to paint, (re)creating its shm if needed. NULL = none free now. */
static struct oxui_buffer *next_buffer(struct oxui_window *w)
{
    struct oxui_buffer *b = pick_free_buffer(w);
    if (!b)
        return NULL;
    if (b->buffer && b->width == w->width && b->height == w->height)
        return b; /* already the right size — reuse */
    /* (Re)create at the requested size. create_shm frees the old buffer only after
     * the new alloc succeeds (non-destructive on OOM). */
    if (create_shm(w, b) == 0)
        return b;
    /* §93b-fix: couldn't allocate the (larger) buffer — DON'T wedge. Fall back to
     * the existing buffer; present() draws at ITS size and the compositor scales it
     * up to the window's display size (blocky, but the app keeps running + animating
     * instead of freezing on a full-screen buffer that won't fit the budget). */
    if (b->buffer)
        return b;
    return NULL; /* nothing to fall back to (first-ever paint OOM) */
}

/* ---- present: call the app's draw into a free buffer and commit (or defer) ---- */

static const struct wl_callback_listener frame_listener;

static void present(struct oxui_window *w)
{
    if (w->wait_for_configure || !w->dirty)
        return;
    struct oxui_buffer *b = next_buffer(w);
    if (!b)
        return; /* §63: both buffers busy — stay dirty, retry on release */

    /* §93b-fix: draw at the BUFFER's size, not w->width/height. They match in the
     * common case; on an OOM fallback the buffer is the old (smaller) size and the
     * compositor scales it — drawing at w->width into a smaller buffer would overflow. */
    oxui_canvas c = { .width = b->width, .height = b->height,
                      .pixels = b->pixels, .time_ms = w->time_ms };
    w->h->draw(w, c, w->user);

    wl_surface_attach(w->surface, b->buffer, 0, 0);
    wl_surface_damage(w->surface, 0, 0, b->width, b->height);

    /* animate mode: ask for a frame callback so the compositor paces us and wakes
     * us for the next frame. event-driven mode: no callback — we sleep until the
     * app marks dirty or an fd fires. */
    if (w->h->animate) {
        struct wl_callback *cb = wl_surface_frame(w->surface);
        wl_callback_add_listener(cb, &frame_listener, w);
    }
    wl_surface_commit(w->surface);
    b->busy = 1;
    w->dirty = 0;
}

static void frame_done(void *data, struct wl_callback *cb, uint32_t time)
{
    struct oxui_window *w = data;
    wl_callback_destroy(cb);
    w->time_ms = time;
    w->dirty = 1; /* animate: paint the next frame */
}
static const struct wl_callback_listener frame_listener = { frame_done };

/* ---- xdg / keyboard / seat / registry plumbing ---- */

static void xdg_surface_configure(void *data, struct xdg_surface *s, uint32_t serial)
{
    struct oxui_window *w = data;
    xdg_surface_ack_configure(s, serial);
    if (w->wait_for_configure) {
        w->wait_for_configure = 0;
        w->dirty = 1;
        present(w);
    }
}
static const struct xdg_surface_listener xdg_surface_listener = { xdg_surface_configure };

static void tl_configure(void *data, struct xdg_toplevel *tl, int32_t width,
                         int32_t height, struct wl_array *states)
{
    struct oxui_window *w = data;
    (void)tl; (void)states;
    if (width > 0 && height > 0 && (width != w->width || height != w->height)) {
        /* §93b: ONLY re-render at the new size if the app can reflow its content
         * (it provides a resize handler, e.g. a terminal that gains rows/cols).
         * Apps with fixed content (a 320x200 game, a fixed animation) keep their
         * small buffer and the compositor scales it — clean (no partial-canvas
         * double-image) AND cheap (no full-resolution render every frame). */
        if (w->h && w->h->resize) {
            w->width = width;
            w->height = height;
            w->dirty = 1;
            w->h->resize(w, width, height, w->user);
        }
    }
}
static void tl_close(void *data, struct xdg_toplevel *tl)
{
    struct oxui_window *w = data;
    (void)tl;
    if (w->h->closed)
        w->h->closed(w, w->user);
    else
        w->running = 0;
}
static const struct xdg_toplevel_listener xdg_toplevel_listener = { tl_configure, tl_close };

static void kb_keymap(void *data, struct wl_keyboard *kb, uint32_t format, int fd, uint32_t size)
{
    struct oxui_window *w = data;
    (void)kb;
    if (format != WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1) { close(fd); return; }
    char *map = mmap(NULL, size, PROT_READ, MAP_SHARED, fd, 0);
    close(fd);
    if (map == MAP_FAILED) return;
    if (!w->xkb_ctx)
        w->xkb_ctx = xkb_context_new(XKB_CONTEXT_NO_DEFAULT_INCLUDES);
    struct xkb_keymap *km = xkb_keymap_new_from_string(
        w->xkb_ctx, map, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    munmap(map, size);
    if (!km) return;
    if (w->xkb_state) xkb_state_unref(w->xkb_state);
    if (w->xkb_keymap) xkb_keymap_unref(w->xkb_keymap);
    w->xkb_keymap = km;
    w->xkb_state = xkb_state_new(km);
}
static void kb_enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *sf, struct wl_array *keys)
{ (void)d;(void)k;(void)s;(void)sf;(void)keys; }
static void kb_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *sf)
{ (void)d;(void)k;(void)s;(void)sf; }
static void kb_key(void *data, struct wl_keyboard *k, uint32_t serial, uint32_t time,
                   uint32_t key, uint32_t state)
{
    struct oxui_window *w = data;
    (void)k; (void)serial; (void)time;
    if (!w->xkb_state) return;
    xkb_keycode_t kc = key + 8; /* Wayland keycode = evdev; xkb is +8 */
    xkb_state_update_key(w->xkb_state, kc, state ? XKB_KEY_DOWN : XKB_KEY_UP);
    if (w->h->key) {
        xkb_keysym_t sym = xkb_state_key_get_one_sym(w->xkb_state, kc);
        w->h->key(w, (uint32_t)sym, state ? 1 : 0, w->user);
    }
}
static void kb_mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t dep,
                    uint32_t lat, uint32_t lck, uint32_t grp)
{ (void)d;(void)k;(void)s;(void)dep;(void)lat;(void)lck;(void)grp; }
static const struct wl_keyboard_listener keyboard_listener = {
    kb_keymap, kb_enter, kb_leave, kb_key, kb_mods,
};

/* Pointer: oxui only forwards button clicks to the app (DOOM maps left-click to fire);
 * enter/leave/motion/axis are accepted but ignored. */
static void pt_enter(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *sf,
                     wl_fixed_t x, wl_fixed_t y)
{ (void)d;(void)p;(void)s;(void)sf;(void)x;(void)y; }
static void pt_leave(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *sf)
{ (void)d;(void)p;(void)s;(void)sf; }
static void pt_motion(void *d, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y)
{ (void)d;(void)p;(void)t;(void)x;(void)y; }
static void pt_button(void *data, struct wl_pointer *p, uint32_t serial, uint32_t time,
                      uint32_t button, uint32_t state)
{
    (void)p; (void)serial; (void)time;
    struct oxui_window *w = data;
    if (w->h && w->h->button)
        w->h->button(w, (int)button, state ? 1 : 0, w->user);
}
static void pt_axis(void *d, struct wl_pointer *p, uint32_t t, uint32_t axis, wl_fixed_t value)
{ (void)d;(void)p;(void)t;(void)axis;(void)value; }
static const struct wl_pointer_listener pointer_listener = {
    pt_enter, pt_leave, pt_motion, pt_button, pt_axis,
};

static void seat_caps(void *data, struct wl_seat *seat, uint32_t caps)
{
    struct oxui_window *w = data;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !w->keyboard) {
        w->keyboard = wl_seat_get_keyboard(seat);
        wl_keyboard_add_listener(w->keyboard, &keyboard_listener, w);
    }
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !w->pointer) {
        w->pointer = wl_seat_get_pointer(seat);
        wl_pointer_add_listener(w->pointer, &pointer_listener, w);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *n) { (void)d;(void)s;(void)n; }
static const struct wl_seat_listener seat_listener = { seat_caps, seat_name };

static void wm_ping(void *d, struct xdg_wm_base *shell, uint32_t serial)
{ (void)d; xdg_wm_base_pong(shell, serial); }
static const struct xdg_wm_base_listener wm_base_listener = { wm_ping };

static void reg_global(void *data, struct wl_registry *r, uint32_t id,
                       const char *iface, uint32_t version)
{
    struct oxui_window *w = data;
    (void)version;
    if (!strcmp(iface, "wl_compositor"))
        w->compositor = wl_registry_bind(r, id, &wl_compositor_interface, 1);
    else if (!strcmp(iface, "xdg_wm_base")) {
        w->wm_base = wl_registry_bind(r, id, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(w->wm_base, &wm_base_listener, w);
    } else if (!strcmp(iface, "wl_seat")) {
        w->seat = wl_registry_bind(r, id, &wl_seat_interface, 1);
        wl_seat_add_listener(w->seat, &seat_listener, w);
    } else if (!strcmp(iface, "wl_shm"))
        w->shm = wl_registry_bind(r, id, &wl_shm_interface, 1);
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t name)
{ (void)d;(void)r;(void)name; }
static const struct wl_registry_listener registry_listener = { reg_global, reg_remove };

/* ---- public API ---- */

oxui_window *oxui_window_create(const char *title, int width, int height)
{
    struct oxui_window *w = zalloc(sizeof *w);
    if (!w) return NULL;
    w->width = width;
    w->height = height;
    w->running = 1;
    wl_list_init(&w->buffer_list);

    w->display = wl_display_connect_to_fd(ox_chan_fd((unsigned)oxui_wl_slot)); /* inherited fd */
    if (!w->display) { free(w); return NULL; }
    w->registry = wl_display_get_registry(w->display);
    wl_registry_add_listener(w->registry, &registry_listener, w);
    wl_display_roundtrip(w->display); /* bind globals */
    wl_display_roundtrip(w->display); /* shm formats etc. */
    if (!w->compositor || !w->shm || !w->wm_base) {
        wl_display_disconnect(w->display);
        free(w);
        return NULL;
    }

    w->surface = wl_compositor_create_surface(w->compositor);
    w->xdg_surface = xdg_wm_base_get_xdg_surface(w->wm_base, w->surface);
    xdg_surface_add_listener(w->xdg_surface, &xdg_surface_listener, w);
    w->xdg_toplevel = xdg_surface_get_toplevel(w->xdg_surface);
    xdg_toplevel_add_listener(w->xdg_toplevel, &xdg_toplevel_listener, w);
    if (title) xdg_toplevel_set_title(w->xdg_toplevel, title);
    wl_surface_commit(w->surface);
    w->wait_for_configure = 1;

    /* pre-allocate the buffer slots (shm created lazily at first paint) */
    for (int i = 0; i < OXUI_NBUF; i++) {
        struct oxui_buffer *b = calloc(1, sizeof *b);
        wl_list_insert(&w->buffer_list, &b->link);
    }
    return w;
}

void oxui_request_redraw(oxui_window *w) { w->dirty = 1; }
void oxui_quit(oxui_window *w) { w->running = 0; }
int  oxui_width(oxui_window *w) { return w->width; }
int  oxui_height(oxui_window *w) { return w->height; }

int oxui_run(oxui_window *w, const oxui_handlers *h, void *user)
{
    w->h = h;
    w->user = user;
    w->dirty = 1; /* force the first paint once configured */

    int wfd = wl_display_get_fd(w->display);
    int efd = h->extra_fd ? h->extra_fd : -1;
    if (!h->extra_fd) efd = -1; /* 0 is stdin; require explicit >0 */

    /* Poll timeout: animate is driven by frame callbacks (block forever, the
     * callback wakes us); a redraw_interval sleeps that long between repaints;
     * otherwise pure event-driven (block forever). §65 */
    int timeout = -1;
    if (!h->animate && h->redraw_interval_ms > 0)
        timeout = h->redraw_interval_ms;

    while (w->running) {
        present(w); /* paint if dirty and a buffer is free */
        wl_display_flush(w->display);

        struct pollfd pfd[2];
        int n = 1;
        pfd[0].fd = wfd; pfd[0].events = POLLIN; pfd[0].revents = 0;
        if (efd > 0) { pfd[1].fd = efd; pfd[1].events = POLLIN; pfd[1].revents = 0; n = 2; }
        int nready = poll(pfd, n, timeout); /* sleeps in the kernel — no busy-poll */

        if (pfd[0].revents & POLLIN) {
            if (wl_display_dispatch(w->display) == -1)
                break;
        }
        if (n == 2 && (pfd[1].revents & POLLIN) && h->fd_ready)
            h->fd_ready(w, w->user);
        /* timed out (no fd ready) and we have an interval → repaint this tick */
        if (nready == 0 && timeout > 0)
            oxui_request_redraw(w);
    }
    return 0;
}

void oxui_window_destroy(oxui_window *w)
{
    struct oxui_buffer *b, *tmp;
    wl_list_for_each_safe(b, tmp, &w->buffer_list, link) {
        if (b->buffer) wl_buffer_destroy(b->buffer);
        if (b->pixels) munmap(b->pixels, b->size);
        wl_list_remove(&b->link);
        free(b);
    }
    if (w->xdg_toplevel) xdg_toplevel_destroy(w->xdg_toplevel);
    if (w->xdg_surface) xdg_surface_destroy(w->xdg_surface);
    if (w->surface) wl_surface_destroy(w->surface);
    wl_display_disconnect(w->display);
    free(w);
}
