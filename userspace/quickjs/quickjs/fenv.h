/* Minimal fenv.h stub for QuickJS */
#ifndef _FENV_H
#define _FENV_H

/* Rounding modes */
#define FE_TONEAREST  0
#define FE_DOWNWARD   1
#define FE_UPWARD     2
#define FE_TOWARDZERO 3

typedef unsigned int fenv_t;
typedef unsigned int fexcept_t;

/* Stub implementations - defined in stubs.c */
int fesetround(int round);
int fegetround(void);

#endif /* _FENV_H */
