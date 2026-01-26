/* stdlib.h shim for bare-metal QuickJS */
#ifndef _STDLIB_H
#define _STDLIB_H

#include "stddef.h"

void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);
void *calloc(size_t nmemb, size_t size);
size_t malloc_usable_size(const void *ptr);

/* Stack allocation - implemented as malloc for simplicity */
void *alloca(size_t size);
#define alloca(size) __builtin_alloca(size)

long strtol(const char *nptr, char **endptr, int base);
long long strtoll(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);
unsigned long long strtoull(const char *nptr, char **endptr, int base);
double strtod(const char *nptr, char **endptr);
float strtof(const char *nptr, char **endptr);
int atoi(const char *nptr);
long atol(const char *nptr);

void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));

void abort(void);
void exit(int status);
void _exit(int status);

char *getenv(const char *name);

int abs(int x);
long labs(long x);
long long llabs(long long x);

#endif /* _STDLIB_H */
