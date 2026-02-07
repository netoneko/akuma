/* unistd.h shim for bare-metal DOOM */
#ifndef _UNISTD_H
#define _UNISTD_H

#include "stddef.h"

/* These are stubs - most are no-ops */
unsigned int sleep(unsigned int seconds);
int usleep(unsigned long usec);
int access(const char *pathname, int mode);
int isatty(int fd);
char *getcwd(char *buf, size_t size);
int chdir(const char *path);
long sysconf(int name);

#define _SC_PAGESIZE 30

#define F_OK 0
#define R_OK 4
#define W_OK 2
#define X_OK 1

#endif /* _UNISTD_H */
