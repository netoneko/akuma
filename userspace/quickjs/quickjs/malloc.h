/* Minimal malloc.h stub for QuickJS */
#ifndef _MALLOC_H
#define _MALLOC_H

#include "stddef.h"

/* These are provided by Rust via FFI */
void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);
void *calloc(size_t nmemb, size_t size);

/* malloc_usable_size stub */
size_t malloc_usable_size(const void *ptr);

#endif /* _MALLOC_H */
