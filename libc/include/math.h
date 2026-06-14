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
double sin(double);
double cos(double);
double tan(double);
double asin(double);
double acos(double);
double atan(double);
double atan2(double, double);
double sinh(double);
double cosh(double);
double tanh(double);
double asinh(double);
double acosh(double);
double atanh(double);
double log10(double);
double log2(double);
double expm1(double);
double trunc(double);
double round(double);
double copysign(double, double);
double modf(double, double *);
double cbrt(double);
double hypot(double, double);

#define HUGE_VAL (1e308 * 10)
#define INFINITY (1e308 * 10)
#define NAN (__builtin_nanf(""))
#define M_PI 3.14159265358979323846
#define M_E 2.7182818284590452354

#define isnan(x) ((x) != (x))
#define isinf(x) ((x) == INFINITY || (x) == -INFINITY)
#define isfinite(x) (!isnan(x) && !isinf(x))
#define signbit(x) (copysign(1.0, (x)) < 0.0)
#define nan(s) (NAN)

#endif
