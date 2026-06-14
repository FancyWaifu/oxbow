#ifndef _SYS_SOCKET_H
#define _SYS_SOCKET_H
#include <stddef_shim.h>
typedef unsigned int socklen_t;
typedef unsigned short sa_family_t;
struct sockaddr { sa_family_t sa_family; char sa_data[14]; };
struct sockaddr_storage { sa_family_t ss_family; char __ss_pad[126]; };
#define AF_INET 2
#define AF_UNSPEC 0
#define PF_INET AF_INET
#define SOCK_STREAM 1
#define SOCK_DGRAM 2
#define SOL_SOCKET 1
#define SO_REUSEADDR 2
#define SO_ERROR 4
#define SO_KEEPALIVE 9
#define SO_RCVBUF 8
#define SO_SNDBUF 7
#define SHUT_RD 0
#define SHUT_WR 1
#define SHUT_RDWR 2
#define MSG_NOSIGNAL 0x4000
int socket(int, int, int);
int connect(int, const struct sockaddr *, socklen_t);
long send(int, const void *, size_t, int);
long recv(int, void *, size_t, int);
int shutdown(int, int);
int setsockopt(int, int, int, const void *, socklen_t);
int getsockopt(int, int, int, void *, socklen_t *);
#endif
