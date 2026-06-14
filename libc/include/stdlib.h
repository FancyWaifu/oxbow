#ifndef _STDLIB_H
#define _STDLIB_H
#include <stddef_shim.h>
void *malloc(size_t);
void *calloc(size_t, size_t);
void *realloc(void *, size_t);
void free(void *);
void exit(int) __attribute__((noreturn));
void abort(void) __attribute__((noreturn));
int atoi(const char *);
long strtol(const char *, char **, int);
unsigned long strtoul(const char *, char **, int);
unsigned long long strtoull(const char *, char **, int);
long long strtoll(const char *, char **, int);
double strtod(const char *, char **);
char *getenv(const char *);
char *realpath(const char *, char *);
void qsort(void *, size_t, size_t, int (*)(const void *, const void *));
int abs(int);
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 0x7fffffff
#endif
