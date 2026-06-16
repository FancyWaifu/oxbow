/* cares-test — a small libc test harness.
 *   cares-test [host]      resolve a hostname via getaddrinfo (c-ares)
 *   cares-test sockpair    exercise AF_UNIX socketpair + SCM_RIGHTS fd passing
 *   cares-test shm         exercise memfd + mmap + passing shared memory by fd
 */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/mman.h>

extern int oxbow_cares_resolve(const char *host, unsigned char out_ip[4]);

/* Create a memfd-backed shared buffer, write a pattern, pass the fd over a
 * socketpair, map the received fd on the other side, and confirm the same
 * memory is seen — the wl_shm buffer-sharing mechanism. */
static int shm_test(void)
{
  int mfd = memfd_create("buf", 0);
  if (mfd < 0 || ftruncate(mfd, 8192) != 0) {
    printf("[shm] memfd/ftruncate failed\n");
    return 1;
  }
  unsigned int *p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
  if (p == MAP_FAILED) {
    printf("[shm] mmap failed\n");
    return 1;
  }
  p[0]    = 0xC0FFEE42;
  p[2047] = 0xDEADBEEF; /* last word of the second page */

  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    printf("[shm] socketpair failed\n");
    return 1;
  }
  char         io[1] = { 'S' };
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
  *(int *)CMSG_DATA(cm) = mfd;
  msg.msg_controllen = cm->cmsg_len;
  sendmsg(sv[0], &msg, 0);

  char          rio[1];
  struct iovec  riov = { rio, 1 };
  struct msghdr rmsg;
  char          rcbuf[CMSG_SPACE(sizeof(int))];
  memset(&rmsg, 0, sizeof rmsg);
  memset(rcbuf, 0, sizeof rcbuf);
  rmsg.msg_iov        = &riov;
  rmsg.msg_iovlen     = 1;
  rmsg.msg_control    = rcbuf;
  rmsg.msg_controllen = sizeof rcbuf;
  recvmsg(sv[1], &rmsg, 0);
  struct cmsghdr *rcm = CMSG_FIRSTHDR(&rmsg);
  if (rcm == 0 || rcm->cmsg_type != SCM_RIGHTS) {
    printf("[shm] FAIL: no fd received\n");
    return 1;
  }
  int rfd = *(int *)CMSG_DATA(rcm);

  unsigned int *q = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, rfd, 0);
  if (q == MAP_FAILED) {
    printf("[shm] FAIL: mmap of received fd failed\n");
    return 1;
  }
  printf("[shm] received fd %d, mapped; q[0]=%08x q[2047]=%08x\n", rfd, q[0], q[2047]);
  if (q[0] == 0xC0FFEE42 && q[2047] == 0xDEADBEEF) {
    /* prove it is the SAME memory, not a copy: write via q, read via p */
    q[1] = 0x1234ABCD;
    if (p[1] == 0x1234ABCD) {
      printf("[shm] OK: memfd shared memory passed by fd (writes visible both ways)\n");
      return 0;
    }
  }
  printf("[shm] FAIL: memory not shared\n");
  return 1;
}

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
  if (argc > 1 && strcmp(argv[1], "shm") == 0) {
    return shm_test();
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
