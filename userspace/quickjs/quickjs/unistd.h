/* Minimal unistd.h stub for QuickJS */
#ifndef _UNISTD_H
#define _UNISTD_H

#include "stddef.h"

#ifndef _SSIZE_T_DEFINED
#define _SSIZE_T_DEFINED
typedef long ssize_t;
#endif

typedef int pid_t;

/* Standard file descriptors */
#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int close(int fd);

#endif /* _UNISTD_H */
