#ifndef _MATH_H
#define _MATH_H
double ldexp(double, int);
double frexp(double, int *);
double fabs(double);
double ldexpl(double, int);
double floor(double);
double ceil(double);
double fmod(double, double);
double sqrt(double);
double exp(double);
double log(double);
double pow(double, double);
#define HUGE_VAL (1e308*10)
#endif
