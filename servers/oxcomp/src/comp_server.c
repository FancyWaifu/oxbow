/* comp_server.c — the compositor half. Advertises wl_compositor + wl_shm, and on
 * a wl_surface.commit copies the attached shm buffer's pixels into the
 * framebuffer. Separate translation unit (server headers) from the client. */
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>   /* read(), ftruncate(), close() */
#include <sys/mman.h> /* mmap for staging the keymap into a memfd */
#include "wayland-server.h"
#include "wayland-server-protocol.h"
#include "xdg-shell-server-protocol.h"
#include "../../oxxkb/xkb/us_keymap.h" /* the US keymap we hand clients (§48) */

extern int memfd_create(const char *name, unsigned int flags);

extern void ox_log(const char *p, unsigned long len);
/* Milliseconds since boot — the frame-callback timestamp clients animate from. */
extern unsigned int ox_now_ms(void);
static void slog(const char *s)
{
  unsigned long n = 0;
  while (s[n])
    n++;
  ox_log(s, n);
}

static unsigned int *g_fb;
static int           g_w, g_h, g_pitch_words;
static struct wl_resource *g_keyboard;  /* the client's wl_keyboard, if bound */
static struct wl_resource *g_focus;     /* the surface holding keyboard focus */
static unsigned int        g_serial;    /* event serial counter */
static int           g_composited;

struct surf {
  struct wl_resource *buffer;       /* pending/current attached wl_buffer */
  struct wl_resource *xdg_surface;  /* the xdg_surface role object, if any */
  struct wl_resource *xdg_toplevel; /* the xdg_toplevel, if any */
  struct wl_resource *frame_cb;     /* pending wl_callback from wl_surface.frame */
  int                 configured;   /* have we sent the initial configure? */
};

/* ---- wl_surface ---------------------------------------------------------- */
static void surface_destroy(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static void surface_attach(struct wl_client *c, struct wl_resource *res,
                           struct wl_resource *buffer, int32_t x, int32_t y)
{
  (void)c;
  (void)x;
  (void)y;
  struct surf *s = wl_resource_get_user_data(res);
  s->buffer = buffer;
}
static void surface_damage(struct wl_client *c, struct wl_resource *res,
                           int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)res; (void)x; (void)y; (void)w; (void)h;
}
static void surface_frame(struct wl_client *c, struct wl_resource *res, uint32_t cb)
{
  struct surf *s = wl_resource_get_user_data(res);
  /* The client asks to be told when it may draw the next frame. Create the
   * wl_callback now; we fire its `done` right after we composite this surface,
   * which drives the client's redraw loop = animation. */
  s->frame_cb = wl_resource_create(c, &wl_callback_interface, 1, cb);
}
static void surface_set_opaque_region(struct wl_client *c, struct wl_resource *res,
                                      struct wl_resource *region)
{
  (void)c; (void)res; (void)region;
}
static void surface_set_input_region(struct wl_client *c, struct wl_resource *res,
                                     struct wl_resource *region)
{
  (void)c; (void)res; (void)region;
}
static void surface_commit(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  struct surf *s = wl_resource_get_user_data(res);
  /* xdg_shell handshake: the client's initial commit (no buffer) asks us to
   * configure it. Reply with a configure; the client acks, draws, and commits
   * again with a buffer. */
  if (s->xdg_surface && s->xdg_toplevel && !s->configured) {
    struct wl_array states;
    wl_array_init(&states);
    xdg_toplevel_send_configure(s->xdg_toplevel, 0, 0, &states);
    wl_array_release(&states);
    xdg_surface_send_configure(s->xdg_surface, 1);
    s->configured = 1;
    return;
  }
  if (!s->buffer)
    return;
  struct wl_shm_buffer *shm = wl_shm_buffer_get(s->buffer);
  if (!shm) {
    slog("[oxcomp/srv] commit: buffer is not wl_shm\n");
    return;
  }
  int bw     = wl_shm_buffer_get_width(shm);
  int bh     = wl_shm_buffer_get_height(shm);
  int stride = wl_shm_buffer_get_stride(shm);
  wl_shm_buffer_begin_access(shm);
  unsigned char *data = wl_shm_buffer_get_data(shm);
  int ox = 140, oy = 130; /* where the window lands on screen */
  for (int y = 0; y < bh && oy + y < g_h; y++) {
    unsigned int *src = (unsigned int *)(data + (long)y * stride);
    for (int x = 0; x < bw && ox + x < g_w; x++) {
      /* shm ARGB8888 and the BGRX framebuffer share byte order — direct copy. */
      g_fb[(long)(oy + y) * g_pitch_words + (ox + x)] = src[x];
    }
  }
  wl_shm_buffer_end_access(shm);
  g_composited = 1;
  /* Give this surface keyboard focus the first time it shows pixels (§47). */
  if (g_keyboard && !g_focus) {
    struct wl_array keys;
    wl_array_init(&keys);
    wl_keyboard_send_enter(g_keyboard, ++g_serial, res, &keys);
    wl_array_release(&keys);
    g_focus = res;
  }
  wl_buffer_send_release(s->buffer); /* client may reuse the buffer */
  /* Tell the client this frame is on screen and it may draw the next one. Its
   * frame-callback handler redraws + commits again → the surface animates. */
  if (s->frame_cb) {
    wl_callback_send_done(s->frame_cb, ox_now_ms());
    wl_resource_destroy(s->frame_cb);
    s->frame_cb = NULL;
  }
}
static void surface_set_buffer_transform(struct wl_client *c, struct wl_resource *r, int32_t t)
{
  (void)c; (void)r; (void)t;
}
static void surface_set_buffer_scale(struct wl_client *c, struct wl_resource *r, int32_t s)
{
  (void)c; (void)r; (void)s;
}
static void surface_damage_buffer(struct wl_client *c, struct wl_resource *r,
                                  int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)r; (void)x; (void)y; (void)w; (void)h;
}
static void surface_offset(struct wl_client *c, struct wl_resource *r, int32_t x, int32_t y)
{
  (void)c; (void)r; (void)x; (void)y;
}
static const struct wl_surface_interface surface_impl = {
  surface_destroy, surface_attach, surface_damage, surface_frame,
  surface_set_opaque_region, surface_set_input_region, surface_commit,
  surface_set_buffer_transform, surface_set_buffer_scale, surface_damage_buffer,
  surface_offset,
};

static void surface_resource_destroy(struct wl_resource *res)
{
  free(wl_resource_get_user_data(res));
}

/* ---- wl_region (no-op; we don't clip) ------------------------------------ */
static void region_destroy(struct wl_client *c, struct wl_resource *r)
{
  (void)c;
  wl_resource_destroy(r);
}
static void region_add(struct wl_client *c, struct wl_resource *r,
                       int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)r; (void)x; (void)y; (void)w; (void)h;
}
static void region_subtract(struct wl_client *c, struct wl_resource *r,
                            int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)r; (void)x; (void)y; (void)w; (void)h;
}
static const struct wl_region_interface region_impl = {
  region_destroy, region_add, region_subtract
};

/* ---- xdg_shell: the standard window protocol (so real apps map) ---------- */
static void noop_destroy(struct wl_client *c, struct wl_resource *r)
{
  (void)c;
  wl_resource_destroy(r);
}

/* xdg_toplevel: window properties — all no-ops for our single fixed window. */
static void tl_set_parent(struct wl_client *c, struct wl_resource *r, struct wl_resource *p)
{ (void)c; (void)r; (void)p; }
static void tl_set_title(struct wl_client *c, struct wl_resource *r, const char *t)
{ (void)c; (void)r; (void)t; }
static void tl_set_app_id(struct wl_client *c, struct wl_resource *r, const char *a)
{ (void)c; (void)r; (void)a; }
static void tl_show_window_menu(struct wl_client *c, struct wl_resource *r,
                                struct wl_resource *seat, uint32_t serial, int32_t x, int32_t y)
{ (void)c; (void)r; (void)seat; (void)serial; (void)x; (void)y; }
static void tl_move(struct wl_client *c, struct wl_resource *r, struct wl_resource *seat, uint32_t s)
{ (void)c; (void)r; (void)seat; (void)s; }
static void tl_resize(struct wl_client *c, struct wl_resource *r, struct wl_resource *seat,
                      uint32_t serial, uint32_t edges)
{ (void)c; (void)r; (void)seat; (void)serial; (void)edges; }
static void tl_set_max_size(struct wl_client *c, struct wl_resource *r, int32_t w, int32_t h)
{ (void)c; (void)r; (void)w; (void)h; }
static void tl_set_min_size(struct wl_client *c, struct wl_resource *r, int32_t w, int32_t h)
{ (void)c; (void)r; (void)w; (void)h; }
static void tl_set_maximized(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static void tl_unset_maximized(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static void tl_set_fullscreen(struct wl_client *c, struct wl_resource *r, struct wl_resource *o)
{ (void)c; (void)r; (void)o; }
static void tl_unset_fullscreen(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static void tl_set_minimized(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static const struct xdg_toplevel_interface toplevel_impl = {
  noop_destroy, tl_set_parent, tl_set_title, tl_set_app_id, tl_show_window_menu,
  tl_move, tl_resize, tl_set_max_size, tl_set_min_size, tl_set_maximized,
  tl_unset_maximized, tl_set_fullscreen, tl_unset_fullscreen, tl_set_minimized,
};

/* xdg_surface */
static void xs_get_toplevel(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct surf        *s = wl_resource_get_user_data(res);
  struct wl_resource *tl =
    wl_resource_create(c, &xdg_toplevel_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(tl, &toplevel_impl, s, NULL);
  s->xdg_toplevel = tl;
}
static void xs_get_popup(struct wl_client *c, struct wl_resource *res, uint32_t id,
                         struct wl_resource *parent, struct wl_resource *positioner)
{ (void)c; (void)res; (void)id; (void)parent; (void)positioner; }
static void xs_set_window_geometry(struct wl_client *c, struct wl_resource *r,
                                   int32_t x, int32_t y, int32_t w, int32_t h)
{ (void)c; (void)r; (void)x; (void)y; (void)w; (void)h; }
static void xs_ack_configure(struct wl_client *c, struct wl_resource *r, uint32_t serial)
{ (void)c; (void)r; (void)serial; }
static const struct xdg_surface_interface xdg_surface_impl = {
  noop_destroy, xs_get_toplevel, xs_get_popup, xs_set_window_geometry, xs_ack_configure,
};

/* xdg_positioner (popups only; minimal — never driven by a toplevel client) */
static const struct xdg_positioner_interface positioner_impl = { 0 };

/* xdg_wm_base */
static void wm_create_positioner(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *p =
    wl_resource_create(c, &xdg_positioner_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(p, &positioner_impl, NULL, NULL);
}
static void wm_get_xdg_surface(struct wl_client *c, struct wl_resource *res, uint32_t id,
                               struct wl_resource *surface)
{
  struct surf        *s = wl_resource_get_user_data(surface);
  struct wl_resource *xs =
    wl_resource_create(c, &xdg_surface_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(xs, &xdg_surface_impl, s, NULL);
  s->xdg_surface = xs;
}
static void wm_pong(struct wl_client *c, struct wl_resource *r, uint32_t serial)
{ (void)c; (void)r; (void)serial; }
static const struct xdg_wm_base_interface wm_base_impl = {
  noop_destroy, wm_create_positioner, wm_get_xdg_surface, wm_pong,
};
static void wm_base_bind(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
  (void)data;
  struct wl_resource *res = wl_resource_create(c, &xdg_wm_base_interface, version, id);
  wl_resource_set_implementation(res, &wm_base_impl, NULL, NULL);
}

/* ---- wl_compositor ------------------------------------------------------- */
static void compositor_create_surface(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct surf        *s = calloc(1, sizeof *s);
  struct wl_resource *sr =
    wl_resource_create(c, &wl_surface_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(sr, &surface_impl, s, surface_resource_destroy);
}
static void compositor_create_region(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *rr =
    wl_resource_create(c, &wl_region_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(rr, &region_impl, NULL, NULL);
}
static const struct wl_compositor_interface compositor_impl = {
  compositor_create_surface, compositor_create_region
};
static void compositor_bind(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
  (void)data;
  struct wl_resource *res = wl_resource_create(c, &wl_compositor_interface, version, id);
  wl_resource_set_implementation(res, &compositor_impl, NULL, NULL);
}

/* ---- wl_seat / wl_keyboard (§47, on-screen input) ----------------------- */
static void keyboard_release(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static const struct wl_keyboard_interface keyboard_impl = { keyboard_release };
static void keyboard_resource_destroy(struct wl_resource *res)
{
  if (g_keyboard == res)
    g_keyboard = NULL;
}
/* Hand the client our keymap (§48): stage the keymap string into a memfd and
 * send it as wl_keyboard.keymap. The client mmaps it and builds an xkb_state, so
 * it decodes keycodes → characters the standard way. */
static void send_keymap(struct wl_resource *kbd)
{
  size_t size = sizeof us_keymap; /* includes the trailing NUL */
  int    fd   = memfd_create("xkb-keymap", 0);
  if (fd < 0)
    return;
  if (ftruncate(fd, (long)size) < 0) {
    close(fd);
    return;
  }
  void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (p == MAP_FAILED) {
    close(fd);
    return;
  }
  memcpy(p, us_keymap, size);
  munmap(p, size);
  wl_keyboard_send_keymap(kbd, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, (uint32_t)size);
  close(fd);
}
static void seat_get_keyboard(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *k =
    wl_resource_create(c, &wl_keyboard_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(k, &keyboard_impl, NULL, keyboard_resource_destroy);
  g_keyboard = k;
  send_keymap(k);
  slog("[oxcomp/srv] wl_keyboard bound (keymap sent)\n");
}
static void seat_get_pointer(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  (void)c; (void)res; (void)id;
}
static void seat_get_touch(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  (void)c; (void)res; (void)id;
}
static void seat_release(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static const struct wl_seat_interface seat_impl = {
  seat_get_pointer, seat_get_keyboard, seat_get_touch, seat_release
};
static void seat_bind(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
  (void)data;
  struct wl_resource *res = wl_resource_create(c, &wl_seat_interface, version, id);
  wl_resource_set_implementation(res, &seat_impl, NULL, NULL);
  wl_seat_send_capabilities(res, WL_SEAT_CAPABILITY_KEYBOARD);
}

/* Event-loop callback: drain the keyboard channel and deliver each set-1 scancode
 * to the focused client as a wl_keyboard.key event (§48). The break bit (0x80)
 * selects press vs release; the low 7 bits ARE the evdev keycode for the main
 * block, which the client offsets by 8 for xkb. We always read() (even with no
 * focus) so the kbd driver's channel never backs up. */
static int on_input(int fd, uint32_t mask, void *data)
{
  (void)mask;
  (void)data;
  unsigned char buf[64];
  long          n = read(fd, buf, sizeof buf);
  for (long i = 0; i < n; i++) {
    if (!g_keyboard || !g_focus)
      continue;
    unsigned char sc      = buf[i];
    uint32_t      keycode = sc & 0x7f;
    uint32_t      state   = (sc & 0x80) ? WL_KEYBOARD_KEY_STATE_RELEASED
                                        : WL_KEYBOARD_KEY_STATE_PRESSED;
    wl_keyboard_send_key(g_keyboard, ++g_serial, ox_now_ms(), keycode, state);
  }
  return 0;
}

/* ---- exported driver entry points --------------------------------------- */
void *comp_server_setup(int fd, int input_fd, unsigned int *fb, int w, int h, int pitch_words)
{
  g_fb = fb;
  g_w = w;
  g_h = h;
  g_pitch_words = pitch_words;
  g_composited = 0;

  /* Paint a desktop background so the screen is self-contained (the client
   * window then composites on top of it). */
  for (int y = 0; y < h; y++)
    for (int x = 0; x < w; x++)
      fb[(long)y * pitch_words + x] = 0x000d3b45; /* deep teal */

  struct wl_display *d = wl_display_create();
  if (!d)
    return NULL;
  wl_global_create(d, &wl_compositor_interface, 4, NULL, compositor_bind);
  wl_global_create(d, &xdg_wm_base_interface, 1, NULL, wm_base_bind);
  wl_global_create(d, &wl_seat_interface, 5, NULL, seat_bind);
  if (wl_display_init_shm(d) < 0) {
    wl_display_destroy(d);
    return NULL;
  }
  /* Watch the keyboard channel fd in the same event loop as the Wayland clients,
   * so the busy-poll dispatch picks up keystrokes (§47). */
  if (input_fd >= 0)
    wl_event_loop_add_fd(wl_display_get_event_loop(d), input_fd, WL_EVENT_READABLE,
                         on_input, d);
  if (!wl_client_create(d, fd)) {
    wl_display_destroy(d);
    return NULL;
  }
  return d;
}

void comp_server_pump(void *d)
{
  struct wl_display *dpy = d;
  wl_event_loop_dispatch(wl_display_get_event_loop(dpy), 0);
  wl_display_flush_clients(dpy);
}

int comp_server_composited(void)
{
  return g_composited;
}
