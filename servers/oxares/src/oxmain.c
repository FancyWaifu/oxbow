/* cares-test — a small libc test harness.
 *   cares-test [host]      resolve a hostname via getaddrinfo (c-ares)
 *   cares-test sockpair    exercise AF_UNIX socketpair + SCM_RIGHTS fd passing
 */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/uio.h>

extern int oxbow_cares_resolve(const char *host, unsigned char out_ip[4]);

static int sockpair_test(void)
{
  int a[2], b[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, a) != 0 ||
      socketpair(AF_UNIX, SOCK_STREAM, 0, b) != 0) {
    printf("[sock] socketpair failed\n");
    return 1;
  }

  /* 1. plain byte stream across the pair */
  write(a[0], "hello", 5);
  char buf[32];
  int  n = read(a[1], buf, sizeof buf);
  printf("[sock] stream: %d bytes \"%.*s\"\n", n, n, buf);

  /* 2. pass fd b[0] across pair A via SCM_RIGHTS */
  char        io[1] = { 'F' };
  struct iovec iov = { io, 1 };
  struct msghdr msg;
  char          cbuf[CMSG_SPACE(sizeof(int))];
  memset(&msg, 0, sizeof msg);
  memset(cbuf, 0, sizeof cbuf);
  msg.msg_iov        = &iov;
  msg.msg_iovlen     = 1;
  msg.msg_control    = cbuf;
  msg.msg_controllen = sizeof cbuf;
  struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
  cm->cmsg_level     = SOL_SOCKET;
  cm->cmsg_type      = SCM_RIGHTS;
  cm->cmsg_len       = CMSG_LEN(sizeof(int));
  *(int *)CMSG_DATA(cm) = b[0];
  msg.msg_controllen = cm->cmsg_len;
  sendmsg(a[0], &msg, 0);

  /* receive the fd on the other end of pair A */
  char         rio[1];
  struct iovec riov = { rio, 1 };
  struct msghdr rmsg;
  char          rcbuf[CMSG_SPACE(sizeof(int))];
  memset(&rmsg, 0, sizeof rmsg);
  memset(rcbuf, 0, sizeof rcbuf);
  rmsg.msg_iov        = &riov;
  rmsg.msg_iovlen     = 1;
  rmsg.msg_control    = rcbuf;
  rmsg.msg_controllen = sizeof rcbuf;
  recvmsg(a[1], &rmsg, 0);
  struct cmsghdr *rcm = CMSG_FIRSTHDR(&rmsg);
  if (rcm == 0 || rcm->cmsg_type != SCM_RIGHTS) {
    printf("[sock] FAIL: no fd in received control message\n");
    return 1;
  }
  int rfd = *(int *)CMSG_DATA(rcm);
  printf("[sock] received fd %d via SCM_RIGHTS\n", rfd);

  /* 3. the passed fd really is b[0]: writing it reaches b[1] */
  write(rfd, "PASS", 4);
  int m = read(b[1], buf, sizeof buf);
  printf("[sock] through passed fd: %d bytes \"%.*s\"\n", m, m, buf);
  if (m == 4 && memcmp(buf, "PASS", 4) == 0) {
    printf("[sock] OK: socketpair + SCM_RIGHTS fd passing works\n");
    return 0;
  }
  printf("[sock] FAIL: passed fd did not carry data\n");
  return 1;
}

int main(int argc, char **argv)
{
  if (argc > 1 && strcmp(argv[1], "sockpair") == 0) {
    return sockpair_test();
  }

  const char *host = (argc > 1) ? argv[1] : "example.com";
  printf("[cares-test] resolving %s via getaddrinfo (c-ares)\n", host);
  unsigned char ip[4] = { 0, 0, 0, 0 };
  if (oxbow_cares_resolve(host, ip)) {
    printf("[cares-test] %s -> %u.%u.%u.%u\n", host, ip[0], ip[1], ip[2], ip[3]);
    return 0;
  }
  printf("[cares-test] %s -> resolution failed\n", host);
  return 1;
}
