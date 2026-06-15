/* wl-test — exercise libwayland's wire layer on oxbow: a wl_connection on each
 * end of an AF_UNIX socketpair, a message written + flushed on one end and read
 * + copied on the other. This drives connection.c + wayland-os.c over our
 * channel-backed fds (the SCM_RIGHTS transport). */
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <wayland-util.h>
#include "wayland-private.h"

int main(void)
{
  printf("[wl-test] libwayland wire core\n");

  /* wayland-util sanity */
  struct wl_array arr;
  wl_array_init(&arr);
  int *p = wl_array_add(&arr, sizeof(int));
  if (p) *p = 42;
  printf("[wl-test] wl_array size=%d val=%d\n", (int)arr.size, p ? *p : -1);
  wl_array_release(&arr);

  /* wl_connection round-trip over a socketpair */
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    printf("[wl-test] socketpair failed\n");
    return 1;
  }
  struct wl_connection *c0 = wl_connection_create(sv[0]);
  struct wl_connection *c1 = wl_connection_create(sv[1]);
  if (!c0 || !c1) {
    printf("[wl-test] wl_connection_create failed\n");
    return 1;
  }

  const char msg[] = "WAYLAND-on-oxbow";
  wl_connection_write(c0, msg, sizeof msg);
  int flushed = wl_connection_flush(c0);
  printf("[wl-test] flushed %d bytes\n", flushed);

  int avail = wl_connection_read(c1);
  printf("[wl-test] read reports %d bytes available\n", avail);

  char buf[64];
  memset(buf, 0, sizeof buf);
  if (avail >= (int)sizeof msg) {
    wl_connection_copy(c1, buf, sizeof msg);
  }
  printf("[wl-test] received \"%s\"\n", buf);

  int ok = (avail >= (int)sizeof msg) && memcmp(buf, msg, sizeof msg) == 0;
  printf("[wl-test] %s\n", ok ? "OK: wl_connection round-trip works" : "FAIL");
  return ok ? 0 : 1;
}
