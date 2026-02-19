#ifndef _STDLIB_H
#define _STDLIB_H

#include "stddef.h"

// Memory
void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);
void *calloc(size_t nmemb, size_t size);

// Conversions
long strtol(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);
long long strtoll(const char *nptr, char **endptr, int base);
unsigned long long strtoull(const char *nptr, char **endptr, int base);
int atoi(const char *nptr);
double strtod(const char *nptr, char **endptr);
float strtof(const char *nptr, char **endptr);
long double strtold(const char *nptr, char **endptr);

// Process control
void exit(int status);
char *getenv(const char *name);
int system(const char *command);
extern char **environ;

// Path resolution
char *realpath(const char *path, char *resolved_path);

void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));

#endif /* _STDLIB_H */
