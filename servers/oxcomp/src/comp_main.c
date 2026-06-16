/* comp_main.c — drives the in-process compositor + client. No Wayland headers
 * here (so it bridges the server-headers TU and the client-headers TU). */
#include <sys/socket.h>
#include <sys/un.h>
#include <fcntl.h>

extern void *comp_server_setup(int fd, unsigned int *fb, int w, int h, int pw);
extern void  comp_server_pump(void *d);
extern int   comp_server_composited(void);
extern void *comp_client_setup(int fd);
extern int   comp_client_draw(void *st);
extern void  comp_client_pump(void *st);
extern int   comp_client_fd(void *st);
extern void  ox_log(const char *p, unsigned long len);

static void logs(const char *s)
{
  unsigned long n = 0;
  while (s[n])
    n++;
  ox_log(s, n);
}

int comp_run(unsigned int *fb, int w, int h, int pitch_words)
{
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    logs("[oxcomp] socketpair failed\n");
    return 0;
  }
  void *server = comp_server_setup(sv[0], fb, w, h, pitch_words);
  if (!server) {
    logs("[oxcomp] server setup failed\n");
    return 0;
  }
  void *client = comp_client_setup(sv[1]);
  if (!client) {
    logs("[oxcomp] client setup failed\n");
    return 0;
  }
  fcntl(comp_client_fd(client), F_SETFL, O_NONBLOCK);

  /* Pump rounds: registry -> bind globals -> draw+commit -> server composites. */
  for (int round = 0; round < 30 && !comp_server_composited(); round++) {
    comp_client_draw(client);
    comp_client_pump(client);
    comp_server_pump(server);
    comp_client_pump(client);
  }
  return comp_server_composited();
}
