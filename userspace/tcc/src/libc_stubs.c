#include "stddef.h"
#include "stdarg.h"
#include "stdio.h"
#include "ctype.h" // For isdigit, isalpha etc.

/* errno global */
int errno = 0;

int *__errno_location(void) {
    return &errno;
}

/* Environment */
char *__environ[] = { NULL };
char **environ = __environ;

/* External functions implemented in Rust */
extern size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
extern int fputc(int c, FILE *stream);
extern void *malloc(size_t size);
extern void *free(void *ptr);
extern void *realloc(void *ptr, size_t size);

/* System configuration */
long sysconf(int name) {
    // TCC uses this to find page size for memory mapping
    if (name == 30) return 4096; // _SC_PAGESIZE
    return -1;
}

/* Assert */
void __assert_fail(const char *assertion, const char *file, unsigned int line, const char *function) {
    printf("Assertion failed: %s, file %s, line %d, function %s\n", assertion, file, line, function);
    while(1);
}

/* Time */
struct tm {
    int tm_sec; int tm_min; int tm_hour; int tm_mday; int tm_mon;
    int tm_year; int tm_wday; int tm_yday; int tm_isdst;
};
struct tm *localtime(const long *timer) {
    static struct tm t = {0}; // Minimal stub
    return &t;
}

/* Math stubs (minimal) */
long double ldexpl(long double x, int exp) {
    // Very simplified ldexpl
    while (exp > 0) { x *= 2.0; exp--; }
    while (exp < 0) { x /= 2.0; exp++; }
    return x;
}

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

char *strpbrk(const char *s, const char *accept) {
    while (*s) {
        const char *a = accept;
        while (*a) {
            if (*s == *a++) return (char *)s;
        }
        s++;
    }
    return NULL;
}

char *realpath(const char *path, char *resolved_path) {
    if (!resolved_path) {
        resolved_path = malloc(1024); // Assuming max path length
        if (!resolved_path) {
            return NULL; // OOM
        }
    }
    strcpy(resolved_path, path);
    return resolved_path;
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

/* Printf family */

/* Helper for vsnprintf - improved to handle width and precision */
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
        
        // Handle flags
        int width = 0;
        int precision = -1;
        int zero_pad = 0;
        int left_justify = 0;
        
        if (*format == '-') { left_justify = 1; format++; }
        if (*format == '0') { zero_pad = 1; format++; }
        
        // Width
        if (*format == '*') {
            width = va_arg(ap, int);
            format++;
        } else {
            while (isdigit(*format)) { width = width * 10 + (*format - '0'); format++; }
        }
        
        // Precision
        if (*format == '.') {
            format++;
            if (*format == '*') {
                precision = va_arg(ap, int);
                format++;
            } else {
                precision = 0;
                while (isdigit(*format)) { precision = precision * 10 + (*format - '0'); format++; }
            }
        }
        
        int is_long = 0;
        int is_longlong = 0;
        if (*format == 'l') { 
            is_long = 1; format++; 
            if (*format == 'l') { is_longlong = 1; format++; } 
        } else if (*format == 'z') {
            is_long = (sizeof(size_t) == sizeof(long));
            is_longlong = (sizeof(size_t) == sizeof(long long));
            format++;
        }
        
        switch (*format) {
            case 's': {
                const char *s = va_arg(ap, const char *);
                if (!s) s = "(null)";
                int len = 0;
                while (s[len] && (precision < 0 || len < precision)) len++;
                
                int pad = width - len;
                if (!left_justify) {
                    while (pad-- > 0 && out < end) *out++ = ' ';
                }
                while (*s && (precision < 0 || precision-- > 0) && out < end) *out++ = *s++;
                if (left_justify) {
                    while (pad-- > 0 && out < end) *out++ = ' ';
                }
                break;
            }
            case 'd':
            case 'i': {
                long long val;
                if (is_longlong) val = va_arg(ap, long long);
                else if (is_long) val = va_arg(ap, long);
                else val = va_arg(ap, int);
                
                char buf[64];
                int neg = val < 0;
                unsigned long long uval = neg ? -val : val;
                int i = 0;
                do { buf[i++] = '0' + (uval % 10); uval /= 10; } while (uval);
                if (neg) buf[i++] = '-';
                
                int pad = width - i;
                if (!left_justify) {
                    while (pad-- > 0 && out < end) *out++ = zero_pad ? '0' : ' ';
                }
                while (i > 0 && out < end) *out++ = buf[--i];
                if (left_justify) {
                    while (pad-- > 0 && out < end) *out++ = ' ';
                }
                break;
            }
            case 'u':
            case 'x': 
            case 'X':
            case 'p': {
                unsigned long long val;
                const char *hex = (*format == 'X') ? "0123456789ABCDEF" : "0123456789abcdef";
                int base = (*format == 'u') ? 10 : 16;
                
                if (*format == 'p') {
                    val = (unsigned long long)va_arg(ap, void*);
                    if (out < end) *out++ = '0';
                    if (out < end) *out++ = 'x';
                    width -= 2;
                } else {
                    if (is_longlong) val = va_arg(ap, unsigned long long);
                    else if (is_long) val = va_arg(ap, unsigned long);
                    else val = va_arg(ap, unsigned int);
                }
                
                char buf[64];
                int i = 0;
                do { 
                    buf[i++] = hex[val % base];
                    val /= base; 
                } while (val);
                
                int pad = width - i;
                if (!left_justify) {
                    while (pad-- > 0 && out < end) *out++ = zero_pad ? '0' : ' ';
                }
                while (i > 0 && out < end) *out++ = buf[--i];
                if (left_justify) {
                    while (pad-- > 0 && out < end) *out++ = ' ';
                }
                break;
            }
            case 'c': {
                char c = (char)va_arg(ap, int);
                if (out < end) *out++ = c;
                break;
            }
            case '%': {
                if (out < end) *out++ = '%';
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

int vprintf(const char *format, va_list ap) {
    return vfprintf(stdout, format, ap);
}

int puts(const char *s) {
    int ret = fprintf(stdout, "%s\n", s);
    return ret >= 0 ? 0 : -1;
}

void abort(void) {
    printf("abort() called\n");
    while(1);
}

int system(const char *command) {
    (void)command;
    return -1; // Not supported
}

/* Simple qsort implementation (bubble sort for simplicity) */
void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *)) {
    char *arr = (char *)base;
    char temp[256]; /* Assumes elements are smaller than 256 bytes */
    
    if (size > sizeof(temp)) return; // Too large to copy
    
    for (size_t i = 0; i < nmemb; i++) {
        for (size_t j = i + 1; j < nmemb; j++) {
            if (compar(arr + i * size, arr + j * size) > 0) {
                memcpy(temp, arr + i * size, size);
                memcpy(arr + i * size, arr + j * size, size);
                memcpy(arr + j * size, temp, size);
            }
        }
    }
}

/* Dynamic loading stubs */
void *dlopen(const char *filename, int flag) { return NULL; }
char *dlerror(void) { return "Dynamic loading not supported"; }
void *dlsym(void *handle, const char *symbol) { return NULL; }
int dlclose(void *handle) { return 0; }