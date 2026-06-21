/* Lean util.h for the oxbow port of sbase: declares only the libutil surface the
 * vendored tools (wc/head/tail) actually use — no regex.h/compat.h (absent on
 * oxbow-libc). The tool .c files and the libutil .c files are verbatim sbase. */
#ifndef OXBOW_SBASE_UTIL_H
#define OXBOW_SBASE_UTIL_H

#include <stddef.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdint.h>
#include <limits.h>
#include <sys/types.h>
#include <sys/stat.h>

#include "arg.h"

#define UTF8_POINT(c) (((c) & 0xc0) != 0x80)

/* clang's freestanding <limits.h> omits SSIZE_MAX; parseoffset.c needs it. */
#ifndef SSIZE_MAX
#define SSIZE_MAX LONG_MAX
#endif

/* clang freestanding <limits.h> omits these POSIX path limits; split/od need them. */
#ifndef NAME_MAX
#define NAME_MAX 255
#endif
#ifndef PATH_MAX
#define PATH_MAX 4096
#endif

/* oxbow-libc's <sys/stat.h> lacks the FIFO bits; tail uses S_ISFIFO. */
#ifndef S_IFIFO
#define S_IFIFO 0010000
#endif
#ifndef S_ISFIFO
#define S_ISFIFO(m) (((m) & S_IFMT) == S_IFIFO)
#endif

#undef MIN
#define MIN(x, y)  ((x) < (y) ? (x) : (y))
#undef MAX
#define MAX(x, y)  ((x) > (y) ? (x) : (y))
#define LEN(x) (sizeof(x) / sizeof *(x))
#define LIMIT(x, a, b)  (x) = (x) < (a) ? (a) : (x) > (b) ? (b) : (x)

extern char *argv0;

void eprintf(const char *, ...);
void weprintf(const char *, ...);
void enprintf(int, const char *, ...);
void xvprintf(const char *, va_list);

long long strtonum(const char *, long long, long long, const char **);
long long enstrtonum(int, const char *, long long, long long);
long long estrtonum(const char *, long long, long long);

int  fshut(FILE *, const char *);
void efshut(FILE *, const char *);
void enfshut(int, FILE *, const char *);

ssize_t writeall(int, const void *, size_t);
int concat(int, const char *, int, const char *);

void *ecalloc(size_t, size_t);
void *emalloc(size_t);
void *erealloc(void *, size_t);
char *estrdup(const char *);
char *estrndup(const char *, size_t);
void *encalloc(int, size_t, size_t);
void *enmalloc(int, size_t);
void *enrealloc(int, void *, size_t);
char *enstrdup(int, const char *);
char *enstrndup(int, const char *, size_t);
void *reallocarray(void *, size_t, size_t);
void *ereallocarray(void *, size_t, size_t);
void *enreallocarray(int, void *, size_t, size_t);

double estrtod(const char *);
off_t parseoffset(const char *);
size_t unescape(char *);

#endif
