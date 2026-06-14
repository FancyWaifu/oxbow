#ifndef _TIME_H
#define _TIME_H
#include <stddef_shim.h>
typedef long clock_t;
time_t time(time_t *);
clock_t clock(void);
struct tm { int tm_sec,tm_min,tm_hour,tm_mday,tm_mon,tm_year,tm_wday,tm_yday,tm_isdst; };
struct tm *localtime(const time_t *);
struct tm *gmtime(const time_t *);
time_t mktime(struct tm *);
size_t strftime(char *, size_t, const char *, const struct tm *);

struct timespec { time_t tv_sec; long tv_nsec; };
#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
#define CLOCK_PROCESS_CPUTIME_ID 2
#define CLOCK_MONOTONIC_RAW 4
#define CLOCK_BOOTTIME 7
int clock_gettime(int, struct timespec *);
#define CLOCKS_PER_SEC 1000
#endif
