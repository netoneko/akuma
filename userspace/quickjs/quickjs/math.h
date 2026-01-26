/* math.h shim for bare-metal QuickJS */
#ifndef _MATH_H
#define _MATH_H

#define HUGE_VAL __builtin_huge_val()
#define NAN __builtin_nan("")
#define INFINITY __builtin_inf()

/* Basic math functions */
double floor(double x);
double ceil(double x);
double fabs(double x);
float fabsf(float x);
double sqrt(double x);
double cbrt(double x);
double pow(double x, double y);
double fmod(double x, double y);
double trunc(double x);
double round(double x);
double rint(double x);
double nearbyint(double x);

/* Exponential and logarithmic */
double exp(double x);
double exp2(double x);
double expm1(double x);
double log(double x);
double log2(double x);
double log10(double x);
double log1p(double x);
double ldexp(double x, int exp);
double frexp(double x, int *exp);
double modf(double x, double *iptr);
double scalbn(double x, int n);

/* Trigonometric */
double sin(double x);
double cos(double x);
double tan(double x);
double asin(double x);
double acos(double x);
double atan(double x);
double atan2(double y, double x);

/* Hyperbolic */
double sinh(double x);
double cosh(double x);
double tanh(double x);
double asinh(double x);
double acosh(double x);
double atanh(double x);

/* Other math functions */
double hypot(double x, double y);
double copysign(double x, double y);
double fmin(double x, double y);
double fmax(double x, double y);

/* Integer conversion */
long lrint(double x);
long long llrint(double x);
long lround(double x);
long long llround(double x);

/* Classification - declare as functions, use builtins for macros */
int isnan(double x);
int isinf(double x);
int isfinite(double x);

/* Use compiler builtins for macros */
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define signbit(x) __builtin_signbit(x)

#endif /* _MATH_H */
