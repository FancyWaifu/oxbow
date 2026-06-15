/* wl-test — an in-process Wayland round-trip on oxbow: a wl_display server (in
 * wl_server_side.c) advertising a wl_compositor global, and a wl_display client
 * here that connects over an AF_UNIX socketpair, fetches the registry, and
 * confirms it receives the global. This drives the WHOLE stack: client + server
 * libs, the event loop (epoll over channel readiness), connection.c, libffi
 * marshalling, all over oxbow's capability transport. */
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include "wayland-client.h"
#include "wayland-client-protocol.h"

extern void *wl_server_setup(int fd);
extern void  wl_server_pump(void *display);
extern void  wl_server_teardown(void *display);

static char  g_iface[64];
static int   g_found;

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version)
{
  (void)data;
  (void)registry;
  (void)name;
  (void)version;
  if (strcmp(interface, "wl_compositor") == 0) {
    strncpy(g_iface, interface, sizeof g_iface - 1);
    g_found = 1;
  }
}
static void registry_global_remove(void *data, struct wl_registry *r, uint32_t n)
{
  (void)data; (void)r; (void)n;
}
static const struct wl_registry_listener registry_listener = {
  registry_global, registry_global_remove
};

int main(void)
{
  printf("[wl-test] in-process Wayland registry round-trip\n");

  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    printf("[wl-test] socketpair failed\n");
    return 1;
  }

  void *server = wl_server_setup(sv[0]);
  if (!server) {
    printf("[wl-test] server setup failed\n");
    return 1;
  }

  printf("[wl-test] server up; connecting client\n");
  struct wl_display *client = wl_display_connect_to_fd(sv[1]);
  if (!client) {
    printf("[wl-test] client connect failed\n");
    return 1;
  }
  struct wl_registry *registry = wl_display_get_registry(client);
  wl_registry_add_listener(registry, &registry_listener, NULL);

  /* Non-blocking display fd so reads never stall (our poll() reports ready
   * unconditionally). Pump a bounded number of rounds: each round flushes the
   * client's queued requests, lets the server process + flush replies, then
   * reads + dispatches whatever arrived. */
  int cfd = wl_display_get_fd(client);
  fcntl(cfd, F_SETFL, O_NONBLOCK);
  for (int round = 0; round < 8 && !g_found; round++) {
    wl_display_flush(client);
    wl_server_pump(server);
    while (wl_display_prepare_read(client) != 0)
      wl_display_dispatch_pending(client);
    wl_display_read_events(client);
    wl_display_dispatch_pending(client);
  }

  if (g_found) {
    printf("[wl-test] registry advertised global: %s\n", g_iface);
    printf("[wl-test] OK: client<->server Wayland round-trip works\n");
  } else {
    printf("[wl-test] FAIL: no global received\n");
  }

  wl_display_disconnect(client);
  wl_server_teardown(server);
  return g_found ? 0 : 1;
}
