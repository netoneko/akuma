/* stdlib.h shim for bare-metal DOOM */
#ifndef _STDLIB_H
#define _STDLIB_H

#include "stddef.h"

void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);
void *calloc(size_t nmemb, size_t size);

long strtol(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);
double strtod(const char *nptr, char **endptr);
int atoi(const char *nptr);
long atol(const char *nptr);
double atof(const char *nptr);

void qsort(void *base, size_t nmemb, size_t size,
            int (*compar)(const void *, const void *));

void abort(void);
void exit(int status);
void _exit(int status);

char *getenv(const char *name);

int abs(int x);
long labs(long x);

int system(const char *command);

#define RAND_MAX 0x7fffffff
int rand(void);
void srand(unsigned int seed);

#endif /* _STDLIB_H */
