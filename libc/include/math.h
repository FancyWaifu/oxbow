#ifndef _MATH_H
#define _MATH_H
double ldexp(double, int);
double frexp(double, int *);
double fabs(double);
double ldexpl(double, int);
#define HUGE_VAL (1e308*10)
#endif
