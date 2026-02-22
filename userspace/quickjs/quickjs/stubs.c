/*
 * Minimal C library implementations for QuickJS on bare-metal
 */

#include "stddef.h"
#include "stdint.h"
#include "stdarg.h"

/* errno global */
int errno = 0;

/* Uptime function - provided by Rust runtime */
extern uint64_t akuma_uptime(void);

/* Exit function - provided by Rust runtime */
extern void akuma_exit(int code);

/* Print function - provided by Rust runtime */
extern void akuma_print(const char *s, size_t len);
/* Abort function - provided by libakuma */
extern void abort(void);

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
    size_t i;
    for (i = 0; i < n && src[i] != '\0'; i++)
        dest[i] = src[i];
    for (; i < n; i++)
        dest[i] = '\0';
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

char *strdup(const char *s) {
    size_t len = strlen(s) + 1;
    extern void *malloc(size_t);
    char *new = malloc(len);
    if (new) memcpy(new, s, len);
    return new;
}

char *strndup(const char *s, size_t n) {
    size_t len = strlen(s);
    if (len > n) len = n;
    extern void *malloc(size_t);
    char *new = malloc(len + 1);
    if (new) {
        memcpy(new, s, len);
        new[len] = '\0';
    }
    return new;
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

int iscntrl(int c) {
    return (c >= 0 && c < 32) || c == 127;
}

int isnan(double x) {
    return x != x;
}

int isinf(double x) {
    return x == __builtin_inf() || x == -__builtin_inf();
}

int isfinite(double x) {
    return !isnan(x) && !isinf(x);
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
    long long result = 0;
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

unsigned long strtoul(const char *nptr, char **endptr, int base) {
    return (unsigned long)strtol(nptr, endptr, base);
}

unsigned long long strtoull(const char *nptr, char **endptr, int base) {
    return (unsigned long long)strtoll(nptr, endptr, base);
}

int atoi(const char *nptr) {
    return (int)strtol(nptr, NULL, 10);
}

long atol(const char *nptr) {
    return strtol(nptr, NULL, 10);
}

/* Floating point stubs - simplified implementations */
double strtod(const char *nptr, char **endptr) {
    double result = 0.0;
    double fraction = 0.0;
    double divisor = 10.0;
    int negative = 0;
    int in_fraction = 0;
    int in_exponent = 0;
    int exp_negative = 0;
    int exponent = 0;
    
    while (isspace(*nptr)) nptr++;
    
    if (*nptr == '-') {
        negative = 1;
        nptr++;
    } else if (*nptr == '+') {
        nptr++;
    }
    
    /* Handle infinity and NaN */
    if (strncmp(nptr, "inf", 3) == 0 || strncmp(nptr, "Inf", 3) == 0 ||
        strncmp(nptr, "INF", 3) == 0) {
        if (endptr) *endptr = (char *)(nptr + 3);
        return negative ? -__builtin_inf() : __builtin_inf();
    }
    if (strncmp(nptr, "nan", 3) == 0 || strncmp(nptr, "NaN", 3) == 0 ||
        strncmp(nptr, "NAN", 3) == 0) {
        if (endptr) *endptr = (char *)(nptr + 3);
        return __builtin_nan("");
    }
    
    while (*nptr) {
        if (*nptr == '.') {
            if (in_fraction || in_exponent) break;
            in_fraction = 1;
            nptr++;
            continue;
        }
        if (*nptr == 'e' || *nptr == 'E') {
            if (in_exponent) break;
            in_exponent = 1;
            nptr++;
            if (*nptr == '-') {
                exp_negative = 1;
                nptr++;
            } else if (*nptr == '+') {
                nptr++;
            }
            continue;
        }
        if (!isdigit(*nptr)) break;
        
        if (in_exponent) {
            exponent = exponent * 10 + (*nptr - '0');
        } else if (in_fraction) {
            fraction += (*nptr - '0') / divisor;
            divisor *= 10.0;
        } else {
            result = result * 10.0 + (*nptr - '0');
        }
        nptr++;
    }
    
    result += fraction;
    
    /* Apply exponent */
    if (exponent != 0) {
        double exp_mult = 1.0;
        for (int i = 0; i < exponent; i++) {
            exp_mult *= 10.0;
        }
        if (exp_negative) {
            result /= exp_mult;
        } else {
            result *= exp_mult;
        }
    }
    
    if (endptr) *endptr = (char *)nptr;
    return negative ? -result : result;
}

float strtof(const char *nptr, char **endptr) {
    return (float)strtod(nptr, endptr);
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

float fabsf(float x) {
    return x < 0 ? -x : x;
}

/* Simplified sqrt using Newton-Raphson */
double sqrt(double x) {
    if (x < 0) return __builtin_nan("");
    if (x == 0) return 0;
    double guess = x / 2.0;
    for (int i = 0; i < 20; i++) {
        guess = (guess + x / guess) / 2.0;
    }
    return guess;
}

double fmod(double x, double y) {
    if (y == 0) return __builtin_nan("");
    return x - (long long)(x / y) * y;
}

double trunc(double x) {
    return (long long)x;
}

double round(double x) {
    return floor(x + 0.5);
}

double rint(double x) {
    return round(x);
}

double nearbyint(double x) {
    return round(x);
}

double copysign(double x, double y) {
    double ax = fabs(x);
    return y < 0 ? -ax : ax;
}

double scalbn(double x, int n) {
    while (n > 0) { x *= 2.0; n--; }
    while (n < 0) { x /= 2.0; n++; }
    return x;
}

/* Stub implementations for math functions QuickJS uses */
double pow(double x, double y) {
    if (y == 0) return 1.0;
    if (y == 1) return x;
    if (y == 2) return x * x;
    if (x == 0) return 0.0;
    
    /* Handle negative exponents */
    if (y < 0) {
        return 1.0 / pow(x, -y);
    }
    
    /* Integer power for common cases */
    if (y == (long long)y) {
        double result = 1.0;
        long long n = (long long)y;
        double base = x;
        while (n > 0) {
            if (n & 1) result *= base;
            base *= base;
            n >>= 1;
        }
        return result;
    }
    
    /* Use exp/log for general case - but we have stubs, so approximate */
    return 0.0;
}

/* Taylor series approximations for trig functions */
double sin(double x) {
    /* Normalize x to [-pi, pi] */
    const double PI = 3.14159265358979323846;
    while (x > PI) x -= 2 * PI;
    while (x < -PI) x += 2 * PI;
    
    double result = x;
    double term = x;
    for (int i = 1; i < 10; i++) {
        term *= -x * x / ((2*i) * (2*i + 1));
        result += term;
    }
    return result;
}

double cos(double x) {
    const double PI = 3.14159265358979323846;
    return sin(x + PI/2);
}

double tan(double x) {
    double c = cos(x);
    if (fabs(c) < 1e-10) return __builtin_inf();
    return sin(x) / c;
}

double asin(double x) {
    /* Simple approximation */
    if (x < -1 || x > 1) return __builtin_nan("");
    double result = x;
    double term = x;
    double x2 = x * x;
    for (int i = 1; i < 10; i++) {
        term *= x2 * (2*i - 1) * (2*i - 1) / ((2*i) * (2*i + 1));
        result += term;
    }
    return result;
}

double acos(double x) {
    const double PI = 3.14159265358979323846;
    return PI/2 - asin(x);
}

double atan(double x) {
    /* Simple approximation for small x */
    if (fabs(x) > 1) {
        const double PI = 3.14159265358979323846;
        if (x > 0) return PI/2 - atan(1/x);
        return -PI/2 - atan(1/x);
    }
    double result = x;
    double term = x;
    double x2 = x * x;
    for (int i = 1; i < 20; i++) {
        term *= -x2;
        result += term / (2*i + 1);
    }
    return result;
}

double atan2(double y, double x) {
    const double PI = 3.14159265358979323846;
    if (x > 0) return atan(y/x);
    if (x < 0 && y >= 0) return atan(y/x) + PI;
    if (x < 0 && y < 0) return atan(y/x) - PI;
    if (x == 0 && y > 0) return PI/2;
    if (x == 0 && y < 0) return -PI/2;
    return 0;
}

/* Exponential and logarithm - Taylor series (must be defined before sinh/cosh/etc) */
double exp(double x) {
    if (x > 700) return __builtin_inf();
    if (x < -700) return 0;
    
    double result = 1.0;
    double term = 1.0;
    for (int i = 1; i < 30; i++) {
        term *= x / i;
        result += term;
        if (fabs(term) < 1e-15) break;
    }
    return result;
}

double log(double x) {
    if (x <= 0) return -__builtin_inf();
    if (x == 1) return 0;
    
    /* Use the identity: log(x) = 2 * atanh((x-1)/(x+1)) */
    double y = (x - 1) / (x + 1);
    double y2 = y * y;
    double result = y;
    double term = y;
    for (int i = 1; i < 30; i++) {
        term *= y2;
        result += term / (2*i + 1);
    }
    return 2 * result;
}

double sinh(double x) {
    double ex = exp(x);
    return (ex - 1/ex) / 2;
}

double cosh(double x) {
    double ex = exp(x);
    return (ex + 1/ex) / 2;
}

double tanh(double x) {
    if (x > 20) return 1.0;
    if (x < -20) return -1.0;
    double ex = exp(2*x);
    return (ex - 1) / (ex + 1);
}

double asinh(double x) {
    return log(x + sqrt(x*x + 1));
}

double acosh(double x) {
    if (x < 1) return __builtin_nan("");
    return log(x + sqrt(x*x - 1));
}

double atanh(double x) {
    if (x <= -1 || x >= 1) return __builtin_nan("");
    return log((1+x)/(1-x)) / 2;
}

double exp2(double x) {
    return pow(2.0, x);
}

double expm1(double x) {
    if (fabs(x) < 1e-5) {
        return x + x*x/2 + x*x*x/6;
    }
    return exp(x) - 1;
}

double log2(double x) {
    return log(x) / 0.693147180559945309417;
}

double log10(double x) {
    return log(x) / 2.302585092994045684018;
}

double log1p(double x) {
    if (fabs(x) < 1e-5) {
        return x - x*x/2 + x*x*x/3;
    }
    return log(1 + x);
}

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

double modf(double x, double *iptr) {
    *iptr = trunc(x);
    return x - *iptr;
}

double cbrt(double x) {
    if (x == 0) return 0;
    int neg = x < 0;
    if (neg) x = -x;
    double guess = x / 3;
    for (int i = 0; i < 20; i++) {
        guess = (2 * guess + x / (guess * guess)) / 3;
    }
    return neg ? -guess : guess;
}

double hypot(double x, double y) {
    return sqrt(x*x + y*y);
}

double fmin(double x, double y) {
    if (isnan(x)) return y;
    if (isnan(y)) return x;
    return x < y ? x : y;
}

double fmax(double x, double y) {
    if (isnan(x)) return y;
    if (isnan(y)) return x;
    return x > y ? x : y;
}

/* Integer rounding functions */
long lrint(double x) {
    return (long)round(x);
}

long long llrint(double x) {
    return (long long)round(x);
}

long lround(double x) {
    return (long)round(x);
}

long long llround(double x) {
    return (long long)round(x);
}

/* qsort implementation using insertion sort (stable, simple) */
void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *)) {
    char *arr = (char *)base;
    char temp[256];
    
    if (size > sizeof(temp)) return;
    
    for (size_t i = 1; i < nmemb; i++) {
        memcpy(temp, arr + i * size, size);
        size_t j = i;
        while (j > 0 && compar(arr + (j-1) * size, temp) > 0) {
            memcpy(arr + j * size, arr + (j-1) * size, size);
            j--;
        }
        memcpy(arr + j * size, temp, size);
    }
}

/* printf/snprintf family */
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
        
        /* Handle flags */
        int left_align = 0;
        int zero_pad = 0;
        int plus_sign = 0;
        int space_sign = 0;
        int hash = 0;
        
        while (1) {
            if (*format == '-') { left_align = 1; format++; }
            else if (*format == '0') { zero_pad = 1; format++; }
            else if (*format == '+') { plus_sign = 1; format++; }
            else if (*format == ' ') { space_sign = 1; format++; }
            else if (*format == '#') { hash = 1; format++; }
            else break;
        }
        (void)left_align; (void)plus_sign; (void)space_sign; (void)hash;
        
        /* Width */
        int width = 0;
        if (*format == '*') {
            width = va_arg(ap, int);
            format++;
        } else {
            while (isdigit(*format)) {
                width = width * 10 + (*format - '0');
                format++;
            }
        }
        
        /* Precision */
        int precision = -1;
        if (*format == '.') {
            format++;
            precision = 0;
            if (*format == '*') {
                precision = va_arg(ap, int);
                format++;
            } else {
                while (isdigit(*format)) {
                    precision = precision * 10 + (*format - '0');
                    format++;
                }
            }
        }
        
        /* Length modifiers */
        int is_long = 0;
        int is_longlong = 0;
        int is_size_t = 0;
        if (*format == 'l') {
            is_long = 1;
            format++;
            if (*format == 'l') {
                is_longlong = 1;
                format++;
            }
        } else if (*format == 'z') {
            is_size_t = 1;
            format++;
        } else if (*format == 'h') {
            format++;
            if (*format == 'h') format++;
        }
        
        switch (*format) {
            case 's': {
                const char *s = va_arg(ap, const char *);
                if (!s) s = "(null)";
                int len = strlen(s);
                if (precision >= 0 && len > precision) len = precision;
                while (len-- > 0 && out < end) *out++ = *s++;
                break;
            }
            case 'd':
            case 'i': {
                long long val;
                if (is_longlong) val = va_arg(ap, long long);
                else if (is_long || is_size_t) val = va_arg(ap, long);
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
                else if (is_long || is_size_t) val = va_arg(ap, unsigned long);
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
                else if (is_long || is_size_t) val = va_arg(ap, unsigned long);
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
            case 'f':
            case 'F':
            case 'e':
            case 'E':
            case 'g':
            case 'G': {
                double val = va_arg(ap, double);
                if (isnan(val)) {
                    const char *s = "nan";
                    while (*s && out < end) *out++ = *s++;
                } else if (isinf(val)) {
                    const char *s = val < 0 ? "-inf" : "inf";
                    while (*s && out < end) *out++ = *s++;
                } else {
                    /* Simple float formatting */
                    if (val < 0) {
                        if (out < end) *out++ = '-';
                        val = -val;
                    }
                    long long int_part = (long long)val;
                    double frac_part = val - int_part;
                    
                    char buf[32];
                    int i = 0;
                    do {
                        buf[i++] = '0' + (int_part % 10);
                        int_part /= 10;
                    } while (int_part);
                    while (i > 0 && out < end) *out++ = buf[--i];
                    
                    if (precision < 0) precision = 6;
                    if (precision > 0) {
                        if (out < end) *out++ = '.';
                        for (int j = 0; j < precision && out < end; j++) {
                            frac_part *= 10;
                            int digit = (int)frac_part;
                            *out++ = '0' + digit;
                            frac_part -= digit;
                        }
                    }
                }
                break;
            }
            case '%':
                if (out < end) *out++ = '%';
                break;
            case 'n': {
                int *ptr = va_arg(ap, int *);
                *ptr = out - str;
                break;
            }
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

int sprintf(char *str, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int result = vsnprintf(str, SIZE_MAX, format, ap);
    va_end(ap);
    return result;
}

int vsprintf(char *str, const char *format, va_list ap) {
    return vsnprintf(str, SIZE_MAX, format, ap);
}

int printf(const char *format, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, format);
    int result = vsnprintf(buf, sizeof(buf), format, ap);
    va_end(ap);
    /* Output via Rust - we'll provide akuma_print */

    akuma_print(buf, result);
    return result;
}

int vprintf(const char *format, va_list ap) {
    char buf[1024];
    int result = vsnprintf(buf, sizeof(buf), format, ap);

    akuma_print(buf, result);
    return result;
}

int fprintf(void *stream, const char *format, ...) {
    (void)stream;
    char buf[1024];
    va_list ap;
    va_start(ap, format);
    int result = vsnprintf(buf, sizeof(buf), format, ap);
    va_end(ap);

    akuma_print(buf, result);
    return result;
}

int puts(const char *s) {
    size_t len = strlen(s);
    akuma_print(s, len);
    akuma_print("\n", 1);
    return 0;
}

int fputs(const char *s, void *stream) {
    (void)stream;
    akuma_print(s, strlen(s));
    return 0;
}

int putchar(int c) {
    char ch = (char)c;
    akuma_print(&ch, 1);
    return c;
}

int fputc(int c, void *stream) {
    (void)stream;
    return putchar(c);
}

/* Memory allocation - calloc */
void *calloc(size_t nmemb, size_t size) {
    extern void *malloc(size_t);
    size_t total = nmemb * size;
    void *ptr = malloc(total);
    if (ptr) memset(ptr, 0, total);
    return ptr;
}

/* malloc_usable_size - we store size in header */
size_t malloc_usable_size(const void *ptr) {
    if (!ptr) return 0;
    /* Size is stored 8 bytes before the pointer (see Rust malloc impl) */
    return *((const size_t *)((const char *)ptr - 8));
}

/* Floating point environment stubs */
int fesetround(int round) {
    (void)round;
    return 0;
}

int fegetround(void) {
    return 0; /* FE_TONEAREST */
}

/* Time functions */
struct timeval {
    long tv_sec;
    long tv_usec;
};

struct timezone {
    int tz_minuteswest;
    int tz_dsttime;
};

int gettimeofday(struct timeval *tv, struct timezone *tz) {
    if (tv) {
        uint64_t uptime = akuma_uptime();
        tv->tv_sec = uptime / 1000000;
        tv->tv_usec = uptime % 1000000;
    }
    if (tz) {
        tz->tz_minuteswest = 0;
        tz->tz_dsttime = 0;
    }
    return 0;
}

/* time_t functions */
typedef long time_t;
typedef long clock_t;

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
    long tm_gmtoff;
    const char *tm_zone;
};

/* Static tm for localtime/gmtime */
static struct tm static_tm;

time_t time(time_t *tloc) {
    uint64_t uptime = akuma_uptime();
    time_t t = uptime / 1000000;
    if (tloc) *tloc = t;
    return t;
}

struct tm *localtime_r(const time_t *timep, struct tm *result) {
    if (!timep || !result) return NULL;
    /* Simple implementation - just return zeros with tm_gmtoff = 0 (UTC) */
    memset(result, 0, sizeof(*result));
    result->tm_gmtoff = 0;
    result->tm_zone = "UTC";
    return result;
}

struct tm *gmtime_r(const time_t *timep, struct tm *result) {
    return localtime_r(timep, result);
}

struct tm *localtime(const time_t *timep) {
    return localtime_r(timep, &static_tm);
}

struct tm *gmtime(const time_t *timep) {
    return gmtime_r(timep, &static_tm);
}

clock_t clock(void) {
    return (clock_t)akuma_uptime();
}

time_t mktime(struct tm *tm) {
    (void)tm;
    return 0;
}

double difftime(time_t time1, time_t time0) {
    return (double)(time1 - time0);
}

size_t strftime(char *s, size_t max, const char *format, const struct tm *tm) {
    (void)s; (void)max; (void)format; (void)tm;
    return 0;
}

/* Pthread stubs (single-threaded environment) */
typedef unsigned long pthread_t;
typedef struct { int dummy; } pthread_mutex_t;
typedef struct { int dummy; } pthread_mutexattr_t;

int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr) {
    (void)mutex; (void)attr;
    return 0;
}

int pthread_mutex_destroy(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_lock(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_unlock(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

pthread_t pthread_self(void) {
    return 1;
}

/* assert */
void __assert_fail(const char *assertion, const char *file, unsigned int line, const char *function) {
    akuma_print("ASSERT FAILED: ", 15);
    if (assertion) akuma_print(assertion, strlen(assertion));
    akuma_print(" in ", 4);
    if (file) akuma_print(file, strlen(file));
    akuma_print("\n", 1);
    abort();
}

/* setjmp/longjmp - minimal stub that just aborts on longjmp */
typedef long jmp_buf[22];

int setjmp(jmp_buf env) {
    (void)env;
    return 0;
}

void longjmp(jmp_buf env, int val) {
    (void)env; (void)val;
    abort();
}

/* abs */
int abs(int x) {
    return x < 0 ? -x : x;
}

long labs(long x) {
    return x < 0 ? -x : x;
}

long long llabs(long long x) {
    return x < 0 ? -x : x;
}

/* getenv stub - always returns NULL */
char *getenv(const char *name) {
    (void)name;
    return NULL;
}

/* FILE type stub */
typedef struct {
    int fd;
    int error;
    int eof;
} FILE;

/* Standard streams - just use dummy pointers */
FILE *stdin = (FILE *)1;
FILE *stdout = (FILE *)2;
FILE *stderr = (FILE *)3;

int fflush(FILE *stream) {
    (void)stream;
    return 0;
}

int feof(FILE *stream) {
    (void)stream;
    return 0;
}

int ferror(FILE *stream) {
    (void)stream;
    return 0;
}

void clearerr(FILE *stream) {
    (void)stream;
}

size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream) {
    (void)stream;

    akuma_print(ptr, size * nmemb);
    return nmemb;
}

size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream) {
    (void)ptr; (void)size; (void)nmemb; (void)stream;
    return 0;
}
