#ifndef _TIME_H
#define _TIME_H
#include <stddef_shim.h>
typedef long clock_t;
time_t time(time_t *);
clock_t clock(void);
struct tm { int tm_sec,tm_min,tm_hour,tm_mday,tm_mon,tm_year,tm_wday,tm_yday,tm_isdst; };
struct tm *localtime(const time_t *);
size_t strftime(char *, size_t, const char *, const struct tm *);
#endif
