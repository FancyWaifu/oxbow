/* comp_server.c — the compositor half. Advertises wl_compositor + wl_shm, and on
 * a wl_surface.commit copies the attached shm buffer's pixels into the
 * framebuffer. Separate translation unit (server headers) from the client. */
#include <stddef.h>
#include <stdlib.h>
#include "wayland-server.h"
#include "wayland-server-protocol.h"

extern void ox_log(const char *p, unsigned long len);
static void slog(const char *s)
{
  unsigned long n = 0;
  while (s[n])
    n++;
  ox_log(s, n);
}

static unsigned int *g_fb;
static int           g_w, g_h, g_pitch_words;
static int           g_composited;

struct surf {
  struct wl_resource *buffer; /* pending/current attached wl_buffer */
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
  (void)c; (void)res; (void)cb;
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
  slog("[oxcomp/srv] commit\n");
  if (!s->buffer) {
    slog("[oxcomp/srv] commit: no buffer attached\n");
    return;
  }
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
  wl_buffer_send_release(s->buffer); /* client may reuse the buffer */
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

/* ---- exported driver entry points --------------------------------------- */
void *comp_server_setup(int fd, unsigned int *fb, int w, int h, int pitch_words)
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
  if (wl_display_init_shm(d) < 0) {
    wl_display_destroy(d);
    return NULL;
  }
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
