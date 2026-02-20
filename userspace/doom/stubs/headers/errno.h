/* errno.h shim for bare-metal DOOM */
#ifndef _ERRNO_H
#define _ERRNO_H

extern int errno;

#define ENOENT  2
#define EIO     5
#define ENOMEM  12
#define EACCES  13
#define EEXIST  17
#define ENOTDIR 20
#define EINVAL  22
#define EISDIR  21
#define ERANGE  34

#endif /* _ERRNO_H */
