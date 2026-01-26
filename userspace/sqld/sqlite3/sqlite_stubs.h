/*
 * Minimal C library stubs for SQLite on bare-metal
 * 
 * These provide the minimal definitions SQLite needs when compiled
 * with SQLITE_OS_OTHER and without standard library.
 */

#ifndef SQLITE_STUBS_H
#define SQLITE_STUBS_H

/* Basic types */
typedef unsigned long size_t;
typedef long ssize_t;
typedef long ptrdiff_t;
typedef long intptr_t;
typedef unsigned long uintptr_t;

typedef signed char int8_t;
typedef unsigned char uint8_t;
typedef signed short int16_t;
typedef unsigned short uint16_t;
typedef signed int int32_t;
typedef unsigned int uint32_t;
typedef signed long long int64_t;
typedef unsigned long long uint64_t;

#define INT8_MIN (-128)
#define INT16_MIN (-32768)
#define INT32_MIN (-2147483647-1)
#define INT64_MIN (-9223372036854775807LL-1)

#define INT8_MAX 127
#define INT16_MAX 32767
#define INT32_MAX 2147483647
#define INT64_MAX 9223372036854775807LL

#define UINT8_MAX 255
#define UINT16_MAX 65535
#define UINT32_MAX 4294967295U
#define UINT64_MAX 18446744073709551615ULL

#define INTPTR_MAX INT64_MAX
#define INTPTR_MIN INT64_MIN
#define UINTPTR_MAX UINT64_MAX
#define SIZE_MAX UINT64_MAX

#define NULL ((void*)0)

/* va_list for variadic functions */
typedef __builtin_va_list va_list;
#define va_start(ap, last) __builtin_va_start(ap, last)
#define va_end(ap) __builtin_va_end(ap)
#define va_arg(ap, type) __builtin_va_arg(ap, type)
#define va_copy(dest, src) __builtin_va_copy(dest, src)

/* Assertions - disabled for bare metal */
#define assert(x) ((void)0)
#define NDEBUG 1

/* Memory functions - provided by our stubs */
void *memset(void *s, int c, size_t n);
void *memcpy(void *dest, const void *src, size_t n);
void *memmove(void *dest, const void *src, size_t n);
int memcmp(const void *s1, const void *s2, size_t n);

/* String functions */
size_t strlen(const char *s);
int strcmp(const char *s1, const char *s2);
int strncmp(const char *s1, const char *s2, size_t n);
char *strcpy(char *dest, const char *src);
char *strncpy(char *dest, const char *src, size_t n);
char *strcat(char *dest, const char *src);
char *strchr(const char *s, int c);
char *strrchr(const char *s, int c);
char *strstr(const char *haystack, const char *needle);

/* Character functions */
int isspace(int c);
int isdigit(int c);
int isalpha(int c);
int isalnum(int c);
int isupper(int c);
int islower(int c);
int toupper(int c);
int tolower(int c);

/* Conversion functions */
long strtol(const char *nptr, char **endptr, int base);
long long strtoll(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);
unsigned long long strtoull(const char *nptr, char **endptr, int base);
double strtod(const char *nptr, char **endptr);
int atoi(const char *nptr);

/* Math functions */
double floor(double x);
double ceil(double x);
double fabs(double x);
double sqrt(double x);
double pow(double x, double y);
double log(double x);
double log10(double x);
double exp(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double fmod(double x, double y);
double ldexp(double x, int exp);
double frexp(double x, int *exp);

/* These are stubs that do nothing - SQLite should not use them with our config */
#define FILE void
#define stdin ((FILE*)0)
#define stdout ((FILE*)0)
#define stderr ((FILE*)0)
#define EOF (-1)

#define fprintf(...) (0)
#define printf(...) (0)
#define sprintf(...) (0)
#define snprintf our_snprintf
#define vsnprintf our_vsnprintf
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
#define getenv(...) ((char*)0)
#define system(...) (-1)
#define remove(...) (-1)
#define rename(...) (-1)
#define getcwd(...) ((char*)0)
#define chdir(...) (-1)

/* Time stubs */
typedef long time_t;
struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
};
#define time(...) ((time_t)0)
#define localtime(...) ((struct tm*)0)
#define gmtime(...) ((struct tm*)0)
#define strftime(...) (0)

/* setjmp/longjmp stubs */
typedef long jmp_buf[32];
#define setjmp(...) (0)
#define longjmp(...)

/* Misc stubs */
#define qsort our_qsort
void our_qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));

#define abort() while(1){}
#define exit(x) while(1){}

/* Memory allocation - will be provided by our code */
void *malloc(size_t size);
void free(void *ptr);
void *realloc(void *ptr, size_t size);

/* snprintf implementation for SQLite */
int our_snprintf(char *str, size_t size, const char *format, ...);
int our_vsnprintf(char *str, size_t size, const char *format, va_list ap);

#endif /* SQLITE_STUBS_H */
