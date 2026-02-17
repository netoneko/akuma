/* stdlib.h shim for bare-metal SQLite */
#ifndef _STDLIB_H
#define _STDLIB_H

#include "stddef.h"

void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);

long strtol(const char *nptr, char **endptr, int base);
long long strtoll(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);
unsigned long long strtoull(const char *nptr, char **endptr, int base);
double strtod(const char *nptr, char **endptr);
int atoi(const char *nptr);

void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));

#define abort() while(1){}
#define exit(x) while(1){}

#define getenv(x) ((char*)0)

#endif /* _STDLIB_H */
