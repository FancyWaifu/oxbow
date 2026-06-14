#ifndef _ARPA_INET_H
#define _ARPA_INET_H
#include <netinet/in.h>
int inet_pton(int, const char *, void *);
unsigned int inet_addr(const char *);
const char *inet_ntop(int, const void *, char *, socklen_t);
#endif
