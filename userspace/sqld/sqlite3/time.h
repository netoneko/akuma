/* time.h shim for bare-metal SQLite */
#ifndef _TIME_H
#define _TIME_H

#include "stddef.h"

typedef long time_t;
typedef long clock_t;

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

#define time(x) ((time_t)0)
#define localtime(x) ((struct tm*)0)
#define gmtime(x) ((struct tm*)0)
#define strftime(...) (0)
#define clock() ((clock_t)0)
#define CLOCKS_PER_SEC 1000000

#endif /* _TIME_H */
