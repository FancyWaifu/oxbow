/* Minimal byte-based UTF shim for the oxbow port — enough for the verbatim sbase
 * wc.c. Each byte is treated as one rune (C locale), so `wc -m` counts bytes like
 * `-c`. A real libutf port would make `-m` count UTF-8 code points; this is the
 * honest first cut. */
#ifndef OXBOW_SBASE_UTF_H
#define OXBOW_SBASE_UTF_H

#include <stdio.h>

typedef int Rune;
#define Runeerror 0xFFFD
#define UTFmax    4

/* Read one rune (here: one byte) from `fp`; returns its byte length (0 at EOF). */
int efgetrune(Rune *, FILE *, const char *);
int isspacerune(Rune);
/* Decode one rune from a byte buffer (here: one byte); returns bytes consumed. */
int charntorune(Rune *, const char *, size_t);

#endif
