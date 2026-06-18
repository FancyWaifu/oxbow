/* cares_glue.c — bridge c-ares's async socket model onto oxbow's net server.
 *
 * c-ares expects non-blocking BSD-style sockets and an event loop. oxbow has
 * neither, so we register custom socket functions (the deprecated-but-simple
 * 5-callback ares_set_socket_functions) that ride the net server's UDP
 * capability API via the extern "C" helpers in oxudp.rs, and drive resolution
 * synchronously with ares_getsock + ares_process_fd against a deadline.
 *
 * UDP only: with the shared-frame path a DNS answer fits in one datagram, so we
 * never need c-ares's TCP fallback. A SOCK_STREAM asocket returns BAD.
 */
#include "ares_private.h" /* internal types, for the sysconfig stub */

#include <ares.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/uio.h>

/* ---- extern "C" UDP helpers (oxudp.rs) ---------------------------------- */
extern unsigned char *ox_udp_attach(void);
extern long           ox_udp_open(void);
extern int  ox_udp_sendv(unsigned long cap, unsigned int ip, unsigned short port,
                         unsigned long len);
extern long ox_udp_recvv(unsigned long cap);
extern void ox_udp_close(unsigned long cap);
extern unsigned long ox_uptime_ms(void);
extern unsigned int  ox_dns_ip(void); /* packed a<<24|b<<16|c<<8|d */

/* resolv.conf parsing is skipped (we set servers via CSV); satisfy the symbols
 * ares_init_by_sysconfig expects on the generic POSIX path. ares_sysconfig_files.c
 * is not compiled, so we provide both functions it defines. */
ares_status_t ares_init_sysconfig_files(const ares_channel_t *channel,
                                        ares_sysconfig_t     *sysconfig,
                                        ares_bool_t           process_resolvconf)
{
  (void)channel;
  (void)sysconfig;
  (void)process_resolvconf;
  return ARES_SUCCESS;
}

ares_status_t ares_init_by_environment(ares_sysconfig_t *sysconfig)
{
  (void)sysconfig; /* no getenv-based overrides */
  return ARES_SUCCESS;
}

/* §94: ares_parse_sortlist is also defined in the skipped ares_sysconfig_files.c
 * but referenced by ares_init.c. The boot binaries link via ld, which prunes the
 * unreached reference; tcc's archive linker pulls the object in and needs a
 * definition. Stub it empty — DNS address sortlist is an optional resolv.conf
 * feature oxbow never configures. */
ares_status_t ares_parse_sortlist(struct apattern **sortlist, size_t *nsort,
                                  const char *str)
{
  (void)str;
  if (sortlist)
    *sortlist = 0;
  if (nsort)
    *nsort = 0;
  return ARES_SUCCESS;
}

/* §94: getservbyport (port -> /etc/services name) is referenced by
 * ares_getnameinfo.c on the generic path but oxbow ships no service database.
 * Return NULL (the caller falls back to the numeric port). `struct servent` is
 * left incomplete — we only return a pointer to it. */
struct servent;
struct servent *getservbyport(int port, const char *proto)
{
  (void)port;
  (void)proto;
  return 0;
}

/* Event-thread / config-change-watch backends are not compiled (oxbow has no
 * epoll/kqueue and we drive ares_process_fd synchronously). These are only
 * reached via ARES_OPT_EVENT_THREAD, which we never set — stub to satisfy the
 * references in ares_init.c. Untyped args: C links by name, not signature. */
ares_status_t ares_event_thread_init(ares_channel_t *channel)
{
  (void)channel;
  return ARES_ENOTIMP;
}
void ares_event_thread_destroy(ares_channel_t *channel) { (void)channel; }
/* configchg types live in event/ares_event.h (not included here); link by name. */
int  ares_event_configchg_init(void *cc, void *e) { (void)cc; (void)e; return 25; }
void ares_event_configchg_destroy(void *cc) { (void)cc; }

/* ---- fd table: c-ares socket fd -> oxbow UDP cap + connected peer -------- */
#define OX_MAXFD 8
static unsigned char *g_shared; /* shared UDP transfer frame */
static struct {
  int            used;
  unsigned long  cap;
  unsigned int   ip;   /* packed a<<24|b<<16|c<<8|d */
  unsigned short port;
} g_fds[OX_MAXFD];

static ares_socket_t ox_asocket(int domain, int type, int protocol, void *ud)
{
  (void)domain;
  (void)protocol;
  (void)ud;
  if (type != SOCK_DGRAM) {
    return ARES_SOCKET_BAD; /* UDP only */
  }
  for (int i = 0; i < OX_MAXFD; i++) {
    if (!g_fds[i].used) {
      long cap = ox_udp_open();
      if (cap < 0) {
        return ARES_SOCKET_BAD;
      }
      g_fds[i].used = 1;
      g_fds[i].cap  = (unsigned long)cap;
      g_fds[i].ip   = 0;
      g_fds[i].port = 0;
      return i;
    }
  }
  return ARES_SOCKET_BAD;
}

static int ox_aclose(ares_socket_t fd, void *ud)
{
  (void)ud;
  if (fd < 0 || fd >= OX_MAXFD || !g_fds[fd].used) {
    return 0;
  }
  ox_udp_close(g_fds[fd].cap);
  g_fds[fd].used = 0;
  return 0;
}

static int ox_aconnect(ares_socket_t fd, const struct sockaddr *addr,
                       ares_socklen_t len, void *ud)
{
  (void)len;
  (void)ud;
  if (fd < 0 || fd >= OX_MAXFD || !g_fds[fd].used || addr->sa_family != AF_INET) {
    errno = EBADF;
    return -1;
  }
  const struct sockaddr_in *si = (const struct sockaddr_in *)addr;
  const unsigned char      *ip = (const unsigned char *)&si->sin_addr;
  const unsigned char      *pp = (const unsigned char *)&si->sin_port;
  g_fds[fd].ip   = ((unsigned int)ip[0] << 24) | ((unsigned int)ip[1] << 16) |
                 ((unsigned int)ip[2] << 8) | ip[3];
  g_fds[fd].port = (unsigned short)(((unsigned short)pp[0] << 8) | pp[1]);
  return 0; /* "connected" — UDP is connectionless */
}

static ares_ssize_t ox_asendv(ares_socket_t fd, const struct iovec *iov,
                              int iovcnt, void *ud)
{
  (void)ud;
  if (fd < 0 || fd >= OX_MAXFD || !g_fds[fd].used) {
    errno = EBADF;
    return -1;
  }
  unsigned long total = 0;
  for (int i = 0; i < iovcnt; i++) {
    if (total + iov[i].iov_len > 1472) {
      break;
    }
    memcpy(g_shared + total, iov[i].iov_base, iov[i].iov_len);
    total += iov[i].iov_len;
  }
  if (ox_udp_sendv(g_fds[fd].cap, g_fds[fd].ip, g_fds[fd].port, total) != 0) {
    errno = EIO;
    return -1;
  }
  return (ares_ssize_t)total;
}

static ares_ssize_t ox_arecvfrom(ares_socket_t fd, void *buf, size_t buflen,
                                 int flags, struct sockaddr *from,
                                 ares_socklen_t *fromlen, void *ud)
{
  (void)flags;
  (void)ud;
  if (fd < 0 || fd >= OX_MAXFD || !g_fds[fd].used) {
    errno = EBADF;
    return -1;
  }
  long n = ox_udp_recvv(g_fds[fd].cap);
  if (n <= 0) {
    errno = EWOULDBLOCK; /* non-blocking: nothing buffered yet */
    return -1;
  }
  if ((size_t)n > buflen) {
    n = (long)buflen;
  }
  memcpy(buf, g_shared, (size_t)n);
  if (from != NULL && fromlen != NULL &&
      *fromlen >= (ares_socklen_t)sizeof(struct sockaddr_in)) {
    struct sockaddr_in *si = (struct sockaddr_in *)from;
    memset(si, 0, sizeof(*si));
    si->sin_family       = AF_INET;
    unsigned int   ip    = g_fds[fd].ip;
    unsigned char *d     = (unsigned char *)&si->sin_addr;
    unsigned char *pp    = (unsigned char *)&si->sin_port;
    d[0]                 = (unsigned char)(ip >> 24);
    d[1]                 = (unsigned char)(ip >> 16);
    d[2]                 = (unsigned char)(ip >> 8);
    d[3]                 = (unsigned char)ip;
    pp[0]                = (unsigned char)(g_fds[fd].port >> 8);
    pp[1]                = (unsigned char)g_fds[fd].port;
    *fromlen             = (ares_socklen_t)sizeof(struct sockaddr_in);
  }
  return n;
}

static const struct ares_socket_functions g_funcs = {
  ox_asocket, ox_aclose, ox_aconnect, ox_arecvfrom, ox_asendv
};

/* ---- synchronous resolve ------------------------------------------------ */
struct ox_result {
  int          done;
  int          ok;
  unsigned int ip; /* 4 wire-order bytes */
};

static void ox_addrinfo_cb(void *arg, int status, int timeouts,
                           struct ares_addrinfo *res)
{
  (void)timeouts;
  struct ox_result *r = (struct ox_result *)arg;
  r->done             = 1;
  r->ok               = 0;
  if (status == ARES_SUCCESS && res != NULL) {
    for (struct ares_addrinfo_node *node = res->nodes; node != NULL;
         node = node->ai_next) {
      if (node->ai_family == AF_INET) {
        struct sockaddr_in *si = (struct sockaddr_in *)node->ai_addr;
        memcpy(&r->ip, &si->sin_addr, 4);
        r->ok = 1;
        break;
      }
    }
  }
  if (res != NULL) {
    ares_freeaddrinfo(res);
  }
}

/* Resolve `host` to an IPv4 address (4 wire-order bytes in out_ip). Returns 1
 * on success, 0 on failure. Drives c-ares to completion against a 5s deadline. */
int oxbow_cares_resolve(const char *host, unsigned char out_ip[4])
{
  if (g_shared == NULL) {
    g_shared = ox_udp_attach();
    if (g_shared == NULL) {
      return 0;
    }
  }

  ares_channel_t      *ch = NULL;
  struct ares_options  opt;
  memset(&opt, 0, sizeof(opt));
  if (ares_init_options(&ch, &opt, 0) != ARES_SUCCESS) {
    return 0;
  }
  ares_set_socket_functions(ch, &g_funcs, NULL);

  /* Point at the DHCP-leased resolver (overrides the init default). */
  unsigned int d = ox_dns_ip();
  char         csv[16];
  snprintf(csv, sizeof(csv), "%u.%u.%u.%u", (d >> 24) & 0xff, (d >> 16) & 0xff,
           (d >> 8) & 0xff, d & 0xff);
  ares_set_servers_csv(ch, csv);

  struct ares_addrinfo_hints hints;
  memset(&hints, 0, sizeof(hints));
  hints.ai_family   = AF_INET;
  hints.ai_socktype = SOCK_DGRAM;

  struct ox_result r = { 0, 0, 0 };
  ares_getaddrinfo(ch, host, NULL, &hints, ox_addrinfo_cb, &r);

  unsigned long deadline = ox_uptime_ms() + 5000;
  while (!r.done) {
    ares_socket_t socks[ARES_GETSOCK_MAXNUM];
    int           bits = ares_getsock(ch, socks, ARES_GETSOCK_MAXNUM);
    if (bits == 0) {
      /* No sockets yet/anymore — let c-ares run timers, then re-check. */
      ares_process_fd(ch, ARES_SOCKET_BAD, ARES_SOCKET_BAD);
    } else {
      for (int i = 0; i < ARES_GETSOCK_MAXNUM; i++) {
        ares_socket_t rfd = ARES_SOCKET_BAD;
        ares_socket_t wfd = ARES_SOCKET_BAD;
        if (ARES_GETSOCK_READABLE(bits, i)) {
          rfd = socks[i];
        }
        if (ARES_GETSOCK_WRITABLE(bits, i)) {
          wfd = socks[i];
        }
        if (rfd != ARES_SOCKET_BAD || wfd != ARES_SOCKET_BAD) {
          ares_process_fd(ch, rfd, wfd);
        }
      }
    }
    if (ox_uptime_ms() > deadline) {
      break;
    }
  }

  int ok = r.ok;
  if (ok) {
    memcpy(out_ip, &r.ip, 4);
  }
  ares_destroy(ch);
  return ok;
}
