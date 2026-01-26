/* Minimal inttypes.h for QuickJS */
#ifndef _INTTYPES_H
#define _INTTYPES_H

#include "stdint.h"

/* Format macros for printf */
#define PRId32 "d"
#define PRIi32 "i"
#define PRIu32 "u"
#define PRIx32 "x"
#define PRIX32 "X"

#define PRId64 "lld"
#define PRIi64 "lli"
#define PRIu64 "llu"
#define PRIx64 "llx"
#define PRIX64 "llX"

#define PRIdPTR "ld"
#define PRIiPTR "li"
#define PRIuPTR "lu"
#define PRIxPTR "lx"
#define PRIXPTR "lX"

/* Scan macros */
#define SCNd32 "d"
#define SCNi32 "i"
#define SCNu32 "u"
#define SCNx32 "x"

#define SCNd64 "lld"
#define SCNi64 "lli"
#define SCNu64 "llu"
#define SCNx64 "llx"

#endif /* _INTTYPES_H */
