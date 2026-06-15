/* Server half of the in-process Wayland round-trip test. Creates a wl_display,
 * advertises a wl_compositor global, and registers a client on a given fd. The
 * client half (oxmain.c) drives the pump. Kept in its own translation unit so
 * the server headers don't collide with the client headers. */
#include <stddef.h>
#include "wayland-server.h"
#include "wayland-server-protocol.h"

static void compositor_bind(struct wl_client *client, void *data,
                            uint32_t version, uint32_t id)
{
  (void)client;
  (void)data;
  (void)version;
  (void)id;
}

/* Set up the server: display + one global + a client on `fd`. Returns the
 * wl_display (as void* so main can hold it without server headers). */
void *wl_server_setup(int fd)
{
  struct wl_display *display = wl_display_create();
  if (!display)
    return NULL;
  wl_global_create(display, &wl_compositor_interface, 4, NULL, compositor_bind);
  if (!wl_client_create(display, fd)) {
    wl_display_destroy(display);
    return NULL;
  }
  return display;
}

/* One server pump: process pending client input, then flush replies out. */
void wl_server_pump(void *display)
{
  struct wl_display    *d    = display;
  struct wl_event_loop *loop = wl_display_get_event_loop(d);
  wl_event_loop_dispatch(loop, 0);
  wl_display_flush_clients(d);
}

void wl_server_teardown(void *display)
{
  wl_display_destroy(display);
}
