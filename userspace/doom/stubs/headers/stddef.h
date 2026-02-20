/* stddef.h shim for bare-metal DOOM */
#ifndef _STDDEF_H
#define _STDDEF_H

typedef unsigned long size_t;
typedef long ssize_t;
typedef long ptrdiff_t;
typedef int wchar_t;

#define NULL ((void*)0)
#define offsetof(type, member) __builtin_offsetof(type, member)

#endif /* _STDDEF_H */
