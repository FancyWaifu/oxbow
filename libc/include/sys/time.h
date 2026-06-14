#ifndef _SYS_TIME_H
#define _SYS_TIME_H
#include <stddef_shim.h>
struct timeval { time_t tv_sec; long tv_usec; };
int gettimeofday(struct timeval *, void *);
#endif
