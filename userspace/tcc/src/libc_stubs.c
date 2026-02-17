/*
 * Libc stubs for TinyCC on Akuma
 * 
 * Implements string, memory, and printf functions.
 * File I/O and memory allocation are implemented in Rust.
 */

#include "stddef.h"
#include "stdarg.h"
#include "stdio.h"

/* errno global */
int errno = 0;

/* External functions implemented in Rust */
extern size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
extern int fputc(int c, FILE *stream);
extern void *malloc(size_t size);
extern void free(void *ptr);
extern void *realloc(void *ptr, size_t size);

/* Memory functions */
void *memset(void *s, int c, size_t n) {
    unsigned char *p = (unsigned char *)s;
    while (n--) {
        *p++ = (unsigned char)c;
    }
    return s;
}

void *memcpy(void *dest, const void *src, size_t n) {
    unsigned char *d = (unsigned char *)dest;
    const unsigned char *s = (const unsigned char *)src;
    while (n--) {
        *d++ = *s++;
    }
    return dest;
}

void *memmove(void *dest, const void *src, size_t n) {
    unsigned char *d = (unsigned char *)dest;
    const unsigned char *s = (const unsigned char *)src;
    if (d < s) {
        while (n--) {
            *d++ = *s++;
        }
    } else if (d > s) {
        d += n;
        s += n;
        while (n--) {
            *--d = *--s;
        }
    }
    return dest;
}

int memcmp(const void *s1, const void *s2, size_t n) {
    const unsigned char *p1 = (const unsigned char *)s1;
    const unsigned char *p2 = (const unsigned char *)s2;
    while (n--) {
        if (*p1 != *p2) {
            return *p1 - *p2;
        }
        p1++;
        p2++;
    }
    return 0;
}

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = (const unsigned char *)s;
    while (n--) {
        if (*p == (unsigned char)c) return (void *)p;
        p++;
    }
    return 0;
}

/* String functions */
size_t strlen(const char *s) {
    const char *p = s;
    while (*p) p++;
    return p - s;
}

int strcmp(const char *s1, const char *s2) {
    while (*s1 && *s1 == *s2) {
        s1++;
        s2++;
    }
    return (unsigned char)*s1 - (unsigned char)*s2;
}

int strncmp(const char *s1, const char *s2, size_t n) {
    while (n && *s1 && *s1 == *s2) {
        s1++;
        s2++;
        n--;
    }
    if (n == 0) return 0;
    return (unsigned char)*s1 - (unsigned char)*s2;
}

char *strcpy(char *dest, const char *src) {
    char *d = dest;
    while ((*d++ = *src++));
    return dest;
}

char *strncpy(char *dest, const char *src, size_t n) {
    char *d = dest;
    while (n && (*d++ = *src++)) n--;
    while (n--) *d++ = '\0';
    return dest;
}

char *strcat(char *dest, const char *src) {
    char *d = dest;
    while (*d) d++;
    while ((*d++ = *src++));
    return dest;
}

char *strchr(const char *s, int c) {
    while (*s) {
        if (*s == (char)c) return (char *)s;
        s++;
    }
    return (c == '\0') ? (char *)s : NULL;
}

char *strrchr(const char *s, int c) {
    const char *last = NULL;
    while (*s) {
        if (*s == (char)c) last = s;
        s++;
    }
    return (c == '\0') ? (char *)s : (char *)last;
}

char *strstr(const char *haystack, const char *needle) {
    size_t needle_len = strlen(needle);
    if (needle_len == 0) return (char *)haystack;
    while (*haystack) {
        if (strncmp(haystack, needle, needle_len) == 0) {
            return (char *)haystack;
        }
        haystack++;
    }
    return NULL;
}

char *strdup(const char *s) {
    size_t len = strlen(s) + 1;
    char *new = malloc(len);
    if (new) memcpy(new, s, len);
    return new;
}

char *strerror(int errnum) {
    return "error";
}

/* Math stubs (minimal) */
double strtod(const char *nptr, char **endptr) {
    return 0.0; /* TODO */
}

long strtol(const char *nptr, char **endptr, int base) {
    long result = 0;
    int negative = 0;
    while (*nptr == ' ' || *nptr == '	') nptr++;
    if (*nptr == '-') { negative = 1; nptr++; }
    else if (*nptr == '+') nptr++;
    
    if (base == 0) {
        if (*nptr == '0') {
            if (nptr[1] == 'x' || nptr[1] == 'X') { base = 16; nptr += 2; }
            else { base = 8; nptr++; }
        } else base = 10;
    }
    
    while (*nptr) {
        int digit;
        if (*nptr >= '0' && *nptr <= '9') digit = *nptr - '0';
        else if (*nptr >= 'a' && *nptr <= 'z') digit = *nptr - 'a' + 10;
        else if (*nptr >= 'A' && *nptr <= 'Z') digit = *nptr - 'A' + 10;
        else break;
        if (digit >= base) break;
        result = result * base + digit;
        nptr++;
    }
    if (endptr) *endptr = (char *)nptr;
    return negative ? -result : result;
}

unsigned long strtoul(const char *nptr, char **endptr, int base) {
    return (unsigned long)strtol(nptr, endptr, base);
}

int atoi(const char *nptr) {
    return (int)strtol(nptr, NULL, 10);
}

/* Character functions */
int isspace(int c) { return c == ' ' || c == '	' || c == '
' || c == ''; }
int isdigit(int c) { return c >= '0' && c <= '9'; }
int isalpha(int c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
int isalnum(int c) { return isalpha(c) || isdigit(c); }

/* Printf family */

/* Helper for vsnprintf - copied from sqlite_stubs.c */
int vsnprintf(char *str, size_t size, const char *format, va_list ap) {
    char *out = str;
    char *end = str + size - 1;
    if (size == 0) return 0;
    
    while (*format && out < end) {
        if (*format != '%') {
            *out++ = *format++;
            continue;
        }
        format++;
        
        int width = 0;
        int zero_pad = 0;
        if (*format == '0') { zero_pad = 1; format++; }
        while (isdigit(*format)) { width = width * 10 + (*format - '0'); format++; }
        
        int is_long = 0;
        if (*format == 'l') { is_long = 1; format++; if (*format == 'l') format++; }
        
        switch (*format) {
            case 's': {
                const char *s = va_arg(ap, const char *);
                if (!s) s = "(null)";
                while (*s && out < end) *out++ = *s++;
                break;
            }
            case 'd':
            case 'i': {
                long val = is_long ? va_arg(ap, long) : va_arg(ap, int);
                char buf[32];
                int neg = val < 0;
                if (neg) val = -val;
                int i = 0;
                do { buf[i++] = '0' + (val % 10); val /= 10; } while (val);
                if (neg) buf[i++] = '-';
                while (i < width) buf[i++] = zero_pad ? '0' : ' ';
                while (i > 0 && out < end) *out++ = buf[--i];
                break;
            }
            case 'x': 
            case 'p': {
                unsigned long val = is_long ? va_arg(ap, unsigned long) : va_arg(ap, unsigned int);
                if (*format == 'p') { val = (unsigned long)va_arg(ap, void*); }
                char buf[32];
                int i = 0;
                do { 
                    int d = val & 0xF;
                    buf[i++] = (d < 10) ? ('0' + d) : ('a' + d - 10);
                    val >>= 4; 
                } while (val);
                while (i < width) buf[i++] = zero_pad ? '0' : ' ';
                while (i > 0 && out < end) *out++ = buf[--i];
                break;
            }
            case 'c': {
                char c = (char)va_arg(ap, int);
                if (out < end) *out++ = c;
                break;
            }
            default:
                if (out < end) *out++ = '%';
                if (out < end && *format) *out++ = *format;
                break;
        }
        if (*format) format++;
    }
    *out = '\0';
    return out - str;
}

int snprintf(char *str, size_t size, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int ret = vsnprintf(str, size, format, ap);
    va_end(ap);
    return ret;
}

int sprintf(char *str, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int ret = vsnprintf(str, 0xFFFFFFFF, format, ap); // Unsafe but standard-compliant
    va_end(ap);
    return ret;
}

int vfprintf(FILE *stream, const char *format, va_list ap) {
    char buf[1024];
    int len = vsnprintf(buf, sizeof(buf), format, ap);
    if (len > 0) {
        fwrite(buf, 1, len, stream);
    }
    return len;
}

int fprintf(FILE *stream, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int ret = vfprintf(stream, format, ap);
    va_end(ap);
    return ret;
}

int printf(const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int ret = vfprintf(stdout, format, ap);
    va_end(ap);
    return ret;
}

/* Dynamic loading stubs */
void *dlopen(const char *filename, int flag) { return NULL; }
char *dlerror(void) { return "Dynamic loading not supported"; }
void *dlsym(void *handle, const char *symbol) { return NULL; }
int dlclose(void *handle) { return 0; }
