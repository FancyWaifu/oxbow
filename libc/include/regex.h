#ifndef _REGEX_H
#define _REGEX_H
#include <stddef.h>

/* Minimal POSIX regex surface, backed by tiny-regex-c (see
 * userland/sbase/libutil/regex_glue.c). REG_NOSUB match-or-not only; tiny-regex
 * uses a single static compile buffer, so one compiled pattern is live at a time
 * (sufficient for nl's realistic single-section use). */
typedef struct { void *prog; } regex_t;
typedef long regoff_t;
typedef struct { regoff_t rm_so; regoff_t rm_eo; } regmatch_t;

#define REG_EXTENDED 1
#define REG_ICASE    2
#define REG_NOSUB    4
#define REG_NEWLINE  8
#define REG_NOTBOL   16
#define REG_NOTEOL   32

#define REG_NOMATCH  1

int regcomp(regex_t *, const char *, int);
int regexec(const regex_t *, const char *, size_t, regmatch_t *, int);
size_t regerror(int, const regex_t *, char *, size_t);
void regfree(regex_t *);

#endif
