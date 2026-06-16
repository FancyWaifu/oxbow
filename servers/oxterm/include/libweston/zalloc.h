#ifndef WESTON_ZALLOC_H
#define WESTON_ZALLOC_H
#include <stdlib.h>
static inline void *zalloc(size_t size) { return calloc(1, size); }
#endif
