#ifndef _NETINET_IN_H
#define _NETINET_IN_H
#include <sys/socket.h>
typedef unsigned short in_port_t;
typedef unsigned int in_addr_t;
struct in_addr { in_addr_t s_addr; };
struct sockaddr_in { sa_family_t sin_family; in_port_t sin_port; struct in_addr sin_addr; unsigned char sin_zero[8]; };
#define INADDR_ANY 0u
#define INADDR_NONE 0xffffffffu
#define IPPROTO_TCP 6
#define IPPROTO_IP 0
unsigned short htons(unsigned short);
unsigned short ntohs(unsigned short);
unsigned int htonl(unsigned int);
unsigned int ntohl(unsigned int);
#endif
