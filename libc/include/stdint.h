#ifndef _STDINT_H
#define _STDINT_H
#include <stddef_shim.h>

typedef long intmax_t;
typedef unsigned long uintmax_t;

typedef int8_t int_least8_t;
typedef int16_t int_least16_t;
typedef int32_t int_least32_t;
typedef int64_t int_least64_t;
typedef uint8_t uint_least8_t;
typedef uint16_t uint_least16_t;
typedef uint32_t uint_least32_t;
typedef uint64_t uint_least64_t;
typedef int8_t int_fast8_t;
typedef long int_fast16_t;
typedef long int_fast32_t;
typedef long int_fast64_t;
typedef uint8_t uint_fast8_t;
typedef unsigned long uint_fast16_t;
typedef unsigned long uint_fast32_t;
typedef unsigned long uint_fast64_t;

#define INT8_MAX    0x7f
#define INT8_MIN    (-INT8_MAX - 1)
#define UINT8_MAX   0xff
#define INT16_MAX   0x7fff
#define INT16_MIN   (-INT16_MAX - 1)
#define UINT16_MAX  0xffff
#define INT32_MAX   0x7fffffff
#define INT32_MIN   (-INT32_MAX - 1)
#define UINT32_MAX  0xffffffffU
#define INT64_MAX   0x7fffffffffffffffLL
#define INT64_MIN   (-INT64_MAX - 1)
#define UINT64_MAX  0xffffffffffffffffULL

#define INTPTR_MAX  0x7fffffffffffffffL
#define INTPTR_MIN  (-INTPTR_MAX - 1)
#define UINTPTR_MAX 0xffffffffffffffffUL
#define INTMAX_MAX  INT64_MAX
#define INTMAX_MIN  INT64_MIN
#define UINTMAX_MAX UINT64_MAX
#define SIZE_MAX    UINTPTR_MAX
#define PTRDIFF_MAX INTPTR_MAX
#define PTRDIFF_MIN INTPTR_MIN

#define INT8_C(c)    c
#define INT16_C(c)   c
#define INT32_C(c)   c
#define INT64_C(c)   c##LL
#define UINT8_C(c)   c
#define UINT16_C(c)  c
#define UINT32_C(c)  c##U
#define UINT64_C(c)  c##ULL
#define INTMAX_C(c)  c##L
#define UINTMAX_C(c) c##UL

#endif
