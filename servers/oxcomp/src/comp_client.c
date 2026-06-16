/* comp_client.c — the client half. Binds wl_compositor + wl_shm, makes a memfd
 * shm pool (the fd is passed to the compositor via SCM_RIGHTS by libwayland),
 * draws a little window into it, and attaches+commits a surface. Separate TU
 * (client headers) from the compositor. */
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include "wayland-client.h"
#include "wayland-client-protocol.h"

static struct wl_compositor *g_compositor;
static struct wl_shm        *g_shm;

static void reg_global(void *data, struct wl_registry *r, uint32_t name,
                       const char *iface, uint32_t ver)
{
  (void)data;
  (void)ver;
  if (strcmp(iface, "wl_compositor") == 0)
    g_compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
  else if (strcmp(iface, "wl_shm") == 0)
    g_shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n)
{
  (void)d; (void)r; (void)n;
}
static const struct wl_registry_listener reg_listener = { reg_global, reg_remove };

extern void ox_log(const char *p, unsigned long len);
static void clog(const char *s)
{
  unsigned long n = 0;
  while (s[n])
    n++;
  ox_log(s, n);
}

struct client_state {
  struct wl_display *dpy;
  struct wl_surface *surface;
  int                drawn;
};

void *comp_client_setup(int fd)
{
  struct wl_display *dpy = wl_display_connect_to_fd(fd);
  if (!dpy)
    return NULL;
  struct client_state *st = calloc(1, sizeof *st);
  st->dpy = dpy;
  struct wl_registry *reg = wl_display_get_registry(dpy);
  wl_registry_add_listener(reg, &reg_listener, NULL);
  return st;
}

int comp_client_fd(void *state)
{
  struct client_state *st = state;
  return wl_display_get_fd(st->dpy);
}

/* Once the globals are bound, build the shm window and commit it (once). */
int comp_client_draw(void *state)
{
  struct client_state *st = state;
  if (st->drawn)
    return 1;
  if (!g_compositor || !g_shm)
    return 0; /* globals not advertised yet */
  clog("[oxcomp/cli] globals bound; building shm window\n");

  const int W = 200, H = 140, stride = W * 4, size = stride * H;
  int       fd = memfd_create("oxcomp-buf", 0);
  if (fd < 0 || ftruncate(fd, size) != 0)
    return 0;
  unsigned int *px = mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (px == (void *)-1)
    return 0;

  for (int y = 0; y < H; y++) {
    for (int x = 0; x < W; x++) {
      unsigned int c;
      if (x < 2 || x >= W - 2 || y < 2 || y >= H - 2)
        c = 0xFF202020; /* dark border */
      else if (y < 26)
        c = 0xFF2D6CDF; /* blue title bar */
      else
        c = 0xFFF0F0F5; /* light body */
      px[y * W + x] = c;
    }
  }

  struct wl_shm_pool *pool = wl_shm_create_pool(g_shm, fd, size); /* fd -> SCM_RIGHTS */
  struct wl_buffer   *buf =
    wl_shm_pool_create_buffer(pool, 0, W, H, stride, WL_SHM_FORMAT_ARGB8888);
  st->surface = wl_compositor_create_surface(g_compositor);
  wl_surface_attach(st->surface, buf, 0, 0);
  wl_surface_damage(st->surface, 0, 0, W, H);
  wl_surface_commit(st->surface);
  clog("[oxcomp/cli] surface committed\n");
  st->drawn = 1;
  return 1;
}

void comp_client_pump(void *state)
{
  struct client_state *st = state;
  wl_display_flush(st->dpy);
  while (wl_display_prepare_read(st->dpy) != 0)
    wl_display_dispatch_pending(st->dpy);
  wl_display_read_events(st->dpy);
  wl_display_dispatch_pending(st->dpy);
}
