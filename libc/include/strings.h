#ifndef _STRINGS_H
#define _STRINGS_H
#include <stddef.h>
int strcasecmp(const char *, const char *);
int strncasecmp(const char *, const char *, size_t);
void bzero(void *, size_t);
int bcmp(const void *, const void *, size_t);
void bcopy(const void *, void *, size_t);
#endif
