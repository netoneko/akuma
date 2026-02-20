/* sys/stat.h shim for bare-metal DOOM */
#ifndef _SYS_STAT_H
#define _SYS_STAT_H

#include "../stddef.h"

int mkdir(const char *path, int mode);

struct stat {
    unsigned long st_size;
    unsigned int st_mode;
};

#define S_ISDIR(m) (((m) & 0170000) == 0040000)

#endif /* _SYS_STAT_H */
