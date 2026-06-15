#ifndef _NETINET_IN_H
#define _NETINET_IN_H
#include <sys/socket.h>
typedef unsigned short in_port_t;
typedef unsigned int in_addr_t;
struct in_addr { in_addr_t s_addr; };
struct sockaddr_in { sa_family_t sin_family; in_port_t sin_port; struct in_addr sin_addr; unsigned char sin_zero[8]; };
/* IPv6 types: declared so portable code (e.g. c-ares) compiles even though
 * oxbow's net stack is IPv4-only and never produces a real AF_INET6 address. */
struct in6_addr { unsigned char s6_addr[16]; };
struct sockaddr_in6 {
    sa_family_t sin6_family;
    in_port_t sin6_port;
    unsigned int sin6_flowinfo;
    struct in6_addr sin6_addr;
    unsigned int sin6_scope_id;
};
#define INADDR_ANY 0u
#define INADDR_LOOPBACK 0x7f000001u
#define INADDR_NONE 0xffffffffu
#define INET_ADDRSTRLEN 16
#define INET6_ADDRSTRLEN 46
#define IPPROTO_TCP 6
#define IPPROTO_UDP 17
#define IPPROTO_IPV6 41
#define IPPROTO_RAW 255
#define IPPROTO_ICMP 1
#define IPPROTO_IP 0
/* IPv6 address classification macros (so c-ares & friends don't see these as
 * implicit function calls). oxbow is IPv4-only, but the predicates are correct. */
#define IN6_IS_ADDR_MULTICAST(a) ((a)->s6_addr[0] == 0xff)
#define IN6_IS_ADDR_LOOPBACK(a)                                            \
    ((a)->s6_addr[0] == 0 && (a)->s6_addr[1] == 0 && (a)->s6_addr[2] == 0 && \
     (a)->s6_addr[3] == 0 && (a)->s6_addr[4] == 0 && (a)->s6_addr[5] == 0 && \
     (a)->s6_addr[6] == 0 && (a)->s6_addr[7] == 0 && (a)->s6_addr[8] == 0 && \
     (a)->s6_addr[9] == 0 && (a)->s6_addr[10] == 0 && (a)->s6_addr[11] == 0 && \
     (a)->s6_addr[12] == 0 && (a)->s6_addr[13] == 0 && (a)->s6_addr[14] == 0 && \
     (a)->s6_addr[15] == 1)
#define IN6_IS_ADDR_V4MAPPED(a)                                            \
    ((a)->s6_addr[0] == 0 && (a)->s6_addr[1] == 0 && (a)->s6_addr[2] == 0 && \
     (a)->s6_addr[3] == 0 && (a)->s6_addr[4] == 0 && (a)->s6_addr[5] == 0 && \
     (a)->s6_addr[6] == 0 && (a)->s6_addr[7] == 0 && (a)->s6_addr[8] == 0 && \
     (a)->s6_addr[9] == 0 && (a)->s6_addr[10] == 0xff && (a)->s6_addr[11] == 0xff)
#define IN6_IS_ADDR_V4COMPAT(a)                                            \
    ((a)->s6_addr[0] == 0 && (a)->s6_addr[1] == 0 && (a)->s6_addr[2] == 0 && \
     (a)->s6_addr[3] == 0 && (a)->s6_addr[4] == 0 && (a)->s6_addr[5] == 0 && \
     (a)->s6_addr[6] == 0 && (a)->s6_addr[7] == 0 && (a)->s6_addr[8] == 0 && \
     (a)->s6_addr[9] == 0 && (a)->s6_addr[10] == 0 && (a)->s6_addr[11] == 0)
#define IN6_IS_ADDR_LINKLOCAL(a) \
    ((a)->s6_addr[0] == 0xfe && ((a)->s6_addr[1] & 0xc0) == 0x80)
#define IN6_IS_ADDR_SITELOCAL(a) \
    ((a)->s6_addr[0] == 0xfe && ((a)->s6_addr[1] & 0xc0) == 0xc0)
unsigned short htons(unsigned short);
unsigned short ntohs(unsigned short);
unsigned int htonl(unsigned int);
unsigned int ntohl(unsigned int);
#endif
