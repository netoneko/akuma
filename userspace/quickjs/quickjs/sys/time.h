/* Minimal sys/time.h stub for QuickJS */
#ifndef _SYS_TIME_H
#define _SYS_TIME_H

#include "../stddef.h"
#include "../stdint.h"

struct timeval {
    long tv_sec;
    long tv_usec;
};

struct timezone {
    int tz_minuteswest;
    int tz_dsttime;
};

/* Stub - defined in stubs.c */
int gettimeofday(struct timeval *tv, struct timezone *tz);

#endif /* _SYS_TIME_H */
