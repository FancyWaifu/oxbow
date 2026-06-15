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
unsigned short htons(unsigned short);
unsigned short ntohs(unsigned short);
unsigned int htonl(unsigned int);
unsigned int ntohl(unsigned int);
#endif
