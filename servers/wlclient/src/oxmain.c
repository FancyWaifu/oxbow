/* wlclient — a standalone Wayland client, spawned by the oxcomp compositor with
 * one end of a channel as its Wayland socket (the WAYLAND_SOCKET inherited-fd
 * model). It binds wl_compositor + wl_shm, draws a window into a shm buffer, and
 * commits — all in a SEPARATE process from the compositor, exercising the
 * channel's cross-process block/wake. (Step 1: prove cross-process Wayland; the
 * real weston-simple-shm + xdg-shell port builds on this.) */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include "wayland-client.h"
#include "wayland-client-protocol.h"

/* The inherited Wayland-socket channel handle lands at spawn slot 1. */
extern int ox_chan_fd(unsigned int handle);

static struct wl_compositor *g_compositor;
static struct wl_shm        *g_shm;

static void reg_global(void *data, struct wl_registry *r, uint32_t name,
                       const char *iface, uint32_t ver)
{
  (void)data; (void)ver;
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

static void draw_window(void)
{
  const int W = 240, H = 160, stride = W * 4, size = stride * H;
  int       fd = memfd_create("wlclient", 0);
  if (fd < 0 || ftruncate(fd, size) != 0)
    return;
  unsigned int *px = mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (px == (void *)-1)
    return;
  for (int y = 0; y < H; y++) {
    for (int x = 0; x < W; x++) {
      unsigned int c;
      if (x < 2 || x >= W - 2 || y < 2 || y >= H - 2)
        c = 0xFF181818;                 /* border */
      else if (y < 28)
        c = 0xFFB03A4A;                 /* red title bar */
      else if (((x >> 4) ^ (y >> 4)) & 1)
        c = 0xFF20242C;                 /* checkerboard body */
      else
        c = 0xFF2C313C;
      px[y * W + x] = c;
    }
  }
  struct wl_shm_pool *pool = wl_shm_create_pool(g_shm, fd, size);
  struct wl_buffer   *buf =
    wl_shm_pool_create_buffer(pool, 0, W, H, stride, WL_SHM_FORMAT_ARGB8888);
  struct wl_surface *surface = wl_compositor_create_surface(g_compositor);
  wl_surface_attach(surface, buf, 0, 0);
  wl_surface_damage(surface, 0, 0, W, H);
  wl_surface_commit(surface);
}

int main(void)
{
  struct wl_display *dpy = wl_display_connect_to_fd(ox_chan_fd(1));
  if (!dpy) {
    printf("[wlclient] connect failed\n");
    return 1;
  }
  int dfd = wl_display_get_fd(dpy);
  fcntl(dfd, F_SETFL, O_NONBLOCK);
  struct wl_registry *reg = wl_display_get_registry(dpy);
  wl_registry_add_listener(reg, &reg_listener, NULL);

  int drawn = 0, drawn_at = 0;
  for (int i = 0; i < 4000; i++) {
    if (g_compositor && g_shm && !drawn) {
      draw_window();
      drawn = 1;
      drawn_at = i;
    }
    wl_display_flush(dpy);
    while (wl_display_prepare_read(dpy) != 0)
      wl_display_dispatch_pending(dpy);
    wl_display_read_events(dpy);
    wl_display_dispatch_pending(dpy);
    if (drawn && i - drawn_at > 200)
      break; /* committed + flushed; let the compositor consume it, then exit */
  }
  wl_display_flush(dpy);
  return drawn ? 0 : 1;
}
