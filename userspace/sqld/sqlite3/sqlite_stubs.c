/*
 * Minimal C library implementations for SQLite on bare-metal
 */

#include "stddef.h"
#include "stdint.h"
#include "stdarg.h"

/* errno global */
int errno = 0;

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

size_t strspn(const char *s, const char *accept) {
    const char *p = s;
    while (*p) {
        const char *a = accept;
        int found = 0;
        while (*a) {
            if (*p == *a) {
                found = 1;
                break;
            }
            a++;
        }
        if (!found) break;
        p++;
    }
    return p - s;
}

size_t strcspn(const char *s, const char *reject) {
    const char *p = s;
    while (*p) {
        const char *r = reject;
        while (*r) {
            if (*p == *r) return p - s;
            r++;
        }
        p++;
    }
    return p - s;
}

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = (const unsigned char *)s;
    while (n--) {
        if (*p == (unsigned char)c) return (void *)p;
        p++;
    }
    return 0;
}

char *strerror(int errnum) {
    (void)errnum;
    return "error";
}

/* Character functions */
int isspace(int c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v';
}

int isdigit(int c) {
    return c >= '0' && c <= '9';
}

int isalpha(int c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
}

int isalnum(int c) {
    return isalpha(c) || isdigit(c);
}

int isupper(int c) {
    return c >= 'A' && c <= 'Z';
}

int islower(int c) {
    return c >= 'a' && c <= 'z';
}

int toupper(int c) {
    return islower(c) ? c - 32 : c;
}

int tolower(int c) {
    return isupper(c) ? c + 32 : c;
}

int isxdigit(int c) {
    return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

int isprint(int c) {
    return c >= 32 && c <= 126;
}

int isnan(double x) {
    return x != x;
}

int isinf(double x) {
    return x == __builtin_inf() || x == -__builtin_inf();
}

/* Number conversion */
long strtol(const char *nptr, char **endptr, int base) {
    long result = 0;
    int negative = 0;
    
    while (isspace(*nptr)) nptr++;
    
    if (*nptr == '-') {
        negative = 1;
        nptr++;
    } else if (*nptr == '+') {
        nptr++;
    }
    
    if (base == 0) {
        if (*nptr == '0') {
            if (nptr[1] == 'x' || nptr[1] == 'X') {
                base = 16;
                nptr += 2;
            } else {
                base = 8;
                nptr++;
            }
        } else {
            base = 10;
        }
    } else if (base == 16 && *nptr == '0' && (nptr[1] == 'x' || nptr[1] == 'X')) {
        nptr += 2;
    }
    
    while (*nptr) {
        int digit;
        if (isdigit(*nptr)) {
            digit = *nptr - '0';
        } else if (isalpha(*nptr)) {
            digit = tolower(*nptr) - 'a' + 10;
        } else {
            break;
        }
        if (digit >= base) break;
        result = result * base + digit;
        nptr++;
    }
    
    if (endptr) *endptr = (char *)nptr;
    return negative ? -result : result;
}

long long strtoll(const char *nptr, char **endptr, int base) {
    return (long long)strtol(nptr, endptr, base);
}

unsigned long strtoul(const char *nptr, char **endptr, int base) {
    return (unsigned long)strtol(nptr, endptr, base);
}

unsigned long long strtoull(const char *nptr, char **endptr, int base) {
    return (unsigned long long)strtol(nptr, endptr, base);
}

int atoi(const char *nptr) {
    return (int)strtol(nptr, NULL, 10);
}

/* Floating point stubs - simplified implementations */
double strtod(const char *nptr, char **endptr) {
    double result = 0.0;
    double fraction = 0.0;
    double divisor = 10.0;
    int negative = 0;
    int in_fraction = 0;
    
    while (isspace(*nptr)) nptr++;
    
    if (*nptr == '-') {
        negative = 1;
        nptr++;
    } else if (*nptr == '+') {
        nptr++;
    }
    
    while (*nptr) {
        if (*nptr == '.') {
            if (in_fraction) break;
            in_fraction = 1;
            nptr++;
            continue;
        }
        if (!isdigit(*nptr)) break;
        
        if (in_fraction) {
            fraction += (*nptr - '0') / divisor;
            divisor *= 10.0;
        } else {
            result = result * 10.0 + (*nptr - '0');
        }
        nptr++;
    }
    
    result += fraction;
    if (endptr) *endptr = (char *)nptr;
    return negative ? -result : result;
}

double floor(double x) {
    long long i = (long long)x;
    return (x < 0 && x != i) ? i - 1 : i;
}

double ceil(double x) {
    long long i = (long long)x;
    return (x > 0 && x != i) ? i + 1 : i;
}

double fabs(double x) {
    return x < 0 ? -x : x;
}

/* Simplified sqrt using Newton-Raphson */
double sqrt(double x) {
    if (x < 0) return 0;
    if (x == 0) return 0;
    double guess = x / 2.0;
    for (int i = 0; i < 20; i++) {
        guess = (guess + x / guess) / 2.0;
    }
    return guess;
}

double fmod(double x, double y) {
    if (y == 0) return 0;
    return x - (long long)(x / y) * y;
}

/* Stub implementations for functions SQLite may not actually use */
double pow(double x, double y) {
    if (y == 0) return 1.0;
    if (y == 1) return x;
    if (y == 2) return x * x;
    /* Very basic integer power for common cases */
    if (y == (long long)y && y > 0) {
        double result = 1.0;
        for (long long i = 0; i < (long long)y; i++) {
            result *= x;
        }
        return result;
    }
    return 0; /* Fallback */
}

double log(double x) { return 0; }
double log10(double x) { return 0; }
double exp(double x) { return 0; }
double sin(double x) { return 0; }
double cos(double x) { return 0; }
double tan(double x) { return 0; }

double ldexp(double x, int exp) {
    while (exp > 0) { x *= 2.0; exp--; }
    while (exp < 0) { x /= 2.0; exp++; }
    return x;
}

double frexp(double x, int *exp) {
    *exp = 0;
    if (x == 0) return 0;
    while (fabs(x) >= 1.0) { x /= 2.0; (*exp)++; }
    while (fabs(x) < 0.5) { x *= 2.0; (*exp)--; }
    return x;
}

/* Simple qsort implementation (bubble sort for simplicity) */
void our_qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *)) {
    char *arr = (char *)base;
    char temp[256]; /* Assumes elements are smaller than 256 bytes */
    
    if (size > sizeof(temp)) return;
    
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

/* Simple snprintf - handles %s, %d, %x, %p, %c, %% */
int vsnprintf(char *str, size_t size, const char *format, va_list ap) {
    char *out = str;
    char *end = str + size - 1;
    
    while (*format && out < end) {
        if (*format != '%') {
            *out++ = *format++;
            continue;
        }
        format++;
        
        /* Handle flags (simplified) */
        int width = 0;
        int zero_pad = 0;
        if (*format == '0') {
            zero_pad = 1;
            format++;
        }
        while (isdigit(*format)) {
            width = width * 10 + (*format - '0');
            format++;
        }
        
        /* Handle length modifiers */
        int is_long = 0;
        int is_longlong = 0;
        if (*format == 'l') {
            is_long = 1;
            format++;
            if (*format == 'l') {
                is_longlong = 1;
                format++;
            }
        }
        
        switch (*format) {
            case 's': {
                const char *s = va_arg(ap, const char *);
                if (!s) s = "(null)";
                while (*s && out < end) *out++ = *s++;
                break;
            }
            case 'd':
            case 'i': {
                long long val;
                if (is_longlong) val = va_arg(ap, long long);
                else if (is_long) val = va_arg(ap, long);
                else val = va_arg(ap, int);
                
                char buf[32];
                int neg = val < 0;
                if (neg) val = -val;
                int i = 0;
                do {
                    buf[i++] = '0' + (val % 10);
                    val /= 10;
                } while (val);
                if (neg) buf[i++] = '-';
                while (i < width) buf[i++] = zero_pad ? '0' : ' ';
                while (i > 0 && out < end) *out++ = buf[--i];
                break;
            }
            case 'u': {
                unsigned long long val;
                if (is_longlong) val = va_arg(ap, unsigned long long);
                else if (is_long) val = va_arg(ap, unsigned long);
                else val = va_arg(ap, unsigned int);
                
                char buf[32];
                int i = 0;
                do {
                    buf[i++] = '0' + (val % 10);
                    val /= 10;
                } while (val);
                while (i < width) buf[i++] = zero_pad ? '0' : ' ';
                while (i > 0 && out < end) *out++ = buf[--i];
                break;
            }
            case 'x':
            case 'X': {
                unsigned long long val;
                if (is_longlong) val = va_arg(ap, unsigned long long);
                else if (is_long) val = va_arg(ap, unsigned long);
                else val = va_arg(ap, unsigned int);
                
                const char *hex = (*format == 'X') ? "0123456789ABCDEF" : "0123456789abcdef";
                char buf[32];
                int i = 0;
                do {
                    buf[i++] = hex[val & 0xF];
                    val >>= 4;
                } while (val);
                while (i < width) buf[i++] = zero_pad ? '0' : ' ';
                while (i > 0 && out < end) *out++ = buf[--i];
                break;
            }
            case 'p': {
                void *ptr = va_arg(ap, void *);
                unsigned long long val = (unsigned long long)ptr;
                if (out < end) *out++ = '0';
                if (out < end) *out++ = 'x';
                char buf[32];
                int i = 0;
                do {
                    buf[i++] = "0123456789abcdef"[val & 0xF];
                    val >>= 4;
                } while (val);
                while (i > 0 && out < end) *out++ = buf[--i];
                break;
            }
            case 'c': {
                char c = (char)va_arg(ap, int);
                if (out < end) *out++ = c;
                break;
            }
            case '%':
                if (out < end) *out++ = '%';
                break;
            default:
                if (out < end) *out++ = '%';
                if (out < end) *out++ = *format;
                break;
        }
        format++;
    }
    
    *out = '\0';
    return out - str;
}

int snprintf(char *str, size_t size, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int result = vsnprintf(str, size, format, ap);
    va_end(ap);
    return result;
}

int vsprintf(char *str, const char *format, va_list ap) {
    return vsnprintf(str, UINT64_MAX, format, ap);
}

/* 
 * Memory allocation - these will be linked from Rust
 * We declare them extern here so SQLite can use them
 */
