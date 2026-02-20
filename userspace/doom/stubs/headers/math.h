/* math.h shim for bare-metal DOOM */
#ifndef _MATH_H
#define _MATH_H

double ceil(double x);
double floor(double x);
double sqrt(double x);
double fabs(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double atan(double x);
double atan2(double y, double x);
double log(double x);
double log2(double x);
double pow(double x, double y);
double fmod(double x, double y);
double round(double x);
float floorf(float x);
float ceilf(float x);
float sqrtf(float x);
float fabsf(float x);

#define HUGE_VAL (__builtin_huge_val())
#define INFINITY (__builtin_inff())
#define NAN      (__builtin_nanf(""))
#define M_PI     3.14159265358979323846

int isnan(double x);
int isinf(double x);

#endif /* _MATH_H */
