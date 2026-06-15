#ifndef _SYS_SOCKET_H
#define _SYS_SOCKET_H
#include <stddef_shim.h>
typedef unsigned int socklen_t;
typedef unsigned short sa_family_t;
struct sockaddr { sa_family_t sa_family; char sa_data[14]; };
struct sockaddr_storage { sa_family_t ss_family; char __ss_pad[126]; };
#define AF_INET 2
#define AF_INET6 10
#define AF_UNSPEC 0
#define PF_INET AF_INET
#define PF_INET6 AF_INET6
#define SOCK_STREAM 1
#define SOCK_DGRAM 2
#define SOL_SOCKET 1
#define SO_REUSEADDR 2
#define SO_ERROR 4
#define SO_KEEPALIVE 9
#define SO_RCVBUF 8
#define SO_SNDBUF 7
#define SO_PEERCRED 17
struct ucred { int pid; unsigned int uid; unsigned int gid; };
#define SHUT_RD 0
#define SHUT_WR 1
#define SHUT_RDWR 2
#define MSG_NOSIGNAL 0x4000
#define MSG_DONTWAIT 0x40
#define MSG_CMSG_CLOEXEC 0x40000000
#define SOCK_CLOEXEC 02000000
#define SOCK_NONBLOCK 04000
int socket(int, int, int);
int connect(int, const struct sockaddr *, socklen_t);
long send(int, const void *, size_t, int);
long recv(int, void *, size_t, int);
int shutdown(int, int);
int setsockopt(int, int, int, const void *, socklen_t);
int getsockopt(int, int, int, void *, socklen_t *);

/* Ancillary-data message passing (sendmsg/recvmsg + SCM_RIGHTS) — the mechanism
 * Wayland uses to pass fds. On oxbow an fd's backing capability is transferred
 * over the channel, so SCM_RIGHTS = capability passing. Layout matches Linux. */
struct iovec; /* from <sys/uio.h> */
struct msghdr {
    void         *msg_name;
    socklen_t     msg_namelen;
    struct iovec *msg_iov;
    size_t        msg_iovlen;
    void         *msg_control;
    size_t        msg_controllen;
    int           msg_flags;
};
struct cmsghdr {
    size_t cmsg_len;
    int    cmsg_level;
    int    cmsg_type;
};
#define SCM_RIGHTS 1
#define __CMSG_ALIGN(n) (((n) + sizeof(size_t) - 1) & ~(sizeof(size_t) - 1))
#define CMSG_ALIGN(n)   __CMSG_ALIGN(n)
#define CMSG_DATA(c)    ((unsigned char *)((struct cmsghdr *)(c) + 1))
#define CMSG_LEN(n)     (__CMSG_ALIGN(sizeof(struct cmsghdr)) + (n))
#define CMSG_SPACE(n)   (__CMSG_ALIGN(sizeof(struct cmsghdr)) + __CMSG_ALIGN(n))
#define CMSG_FIRSTHDR(m)                                                       \
    ((size_t)(m)->msg_controllen >= sizeof(struct cmsghdr)                     \
         ? (struct cmsghdr *)(m)->msg_control                                  \
         : (struct cmsghdr *)0)
#define CMSG_NXTHDR(m, c)                                                      \
    (((unsigned char *)(c) + __CMSG_ALIGN((c)->cmsg_len) +                     \
          __CMSG_ALIGN(sizeof(struct cmsghdr)) >                               \
      (unsigned char *)(m)->msg_control + (m)->msg_controllen)                 \
         ? (struct cmsghdr *)0                                                 \
         : (struct cmsghdr *)((unsigned char *)(c) +                          \
                              __CMSG_ALIGN((c)->cmsg_len)))

int socketpair(int, int, int, int[2]);
long sendmsg(int, const struct msghdr *, int);
long recvmsg(int, struct msghdr *, int);
#endif
