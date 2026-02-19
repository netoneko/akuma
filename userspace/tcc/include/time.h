/* time.h shim for bare-metal SQLite */
#ifndef _TIME_H
#define _TIME_H

#include "stddef.h"

// typedef long long time_t; // Removed
typedef long long clock_t; // Keep this consistent

struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
};

// time_t time(time_t *tloc); // Will be defined by -Dtime_t
time_t time(time_t *tloc);

#define localtime(x) ((struct tm*)0)
#define gmtime(x) ((struct tm*)0)
#define strftime(...) (0)
#define clock() ((clock_t)0)
#define CLOCKS_PER_SEC 1000000

#endif /* _TIME_H */
