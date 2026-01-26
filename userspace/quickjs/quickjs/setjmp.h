/* Minimal setjmp.h stub for QuickJS */
#ifndef _SETJMP_H
#define _SETJMP_H

/* jmp_buf needs to be large enough to store registers */
typedef long jmp_buf[22];
typedef long sigjmp_buf[22];

int setjmp(jmp_buf env);
void longjmp(jmp_buf env, int val);

#define sigsetjmp(env, save) setjmp(env)
#define siglongjmp(env, val) longjmp(env, val)

#endif /* _SETJMP_H */
