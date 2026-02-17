/* math.h shim for bare-metal SQLite */
#ifndef _MATH_H
#define _MATH_H

double floor(double x);
double ceil(double x);
double fabs(double x);
double sqrt(double x);
double pow(double x, double y);
double log(double x);
double log10(double x);
double exp(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double fmod(double x, double y);
double ldexp(double x, int exp);
double frexp(double x, int *exp);

#define ldexpl(x, e) ldexp((double)(x), (e))

#define HUGE_VAL __builtin_huge_val()
#define NAN __builtin_nan("")
#define INFINITY __builtin_inf()

int isnan(double x);
int isinf(double x);

#endif /* _MATH_H */
