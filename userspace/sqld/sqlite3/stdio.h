/* stdio.h shim for bare-metal SQLite */
#ifndef _STDIO_H
#define _STDIO_H

#include "stddef.h"
#include "stdarg.h"

#define FILE void
#define stdin ((FILE*)0)
#define stdout ((FILE*)0)
#define stderr ((FILE*)0)
#define EOF (-1)

#define fprintf(...) (0)
#define printf(...) (0)
#define sprintf(...) (0)

/* snprintf and vsnprintf are implemented in sqlite_stubs.c */
int snprintf(char *str, size_t size, const char *format, ...);
int vsnprintf(char *str, size_t size, const char *format, va_list ap);
int vsprintf(char *str, const char *format, va_list ap);
#define sscanf(...) (0)
#define fopen(...) ((FILE*)0)
#define fclose(...) (0)
#define fread(...) (0)
#define fwrite(...) (0)
#define fflush(...) (0)
#define fputs(...) (0)
#define fgets(...) ((char*)0)
#define fseek(...) (-1)
#define ftell(...) (-1)
#define rewind(...)
#define ferror(...) (0)
#define feof(...) (1)
#define ungetc(...) (EOF)
#define getc(...) (EOF)
#define putc(...) (EOF)
#define remove(...) (-1)
#define rename(...) (-1)

#endif /* _STDIO_H */
