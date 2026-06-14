#ifndef _NET_IF_H
#define _NET_IF_H
unsigned int if_nametoindex(const char *);
char *if_indextoname(unsigned int, char *);
#define IF_NAMESIZE 16
#endif
