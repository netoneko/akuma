#ifndef _SETJMP_H
#define _SETJMP_H

typedef long long jmp_buf[22];

int setjmp(jmp_buf env);
void longjmp(jmp_buf env, int val);

#endif
