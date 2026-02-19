/*
 * Minimal C library stubs for DOOM on bare-metal Akuma
 *
 * Provides memory, string, stdio (file I/O via syscalls), ctype,
 * math, and misc functions needed by doomgeneric.
 */

#include "stddef.h"
#include "stdint.h"
#include "stdarg.h"
#include "stdio.h"

/* ========================================================================= */
/* Extern Rust FFI functions (provided by main.rs)                           */
/* ========================================================================= */

extern void *akuma_malloc(size_t size);
extern void akuma_free(void *ptr);
extern void *akuma_realloc(void *ptr, size_t size);
extern void akuma_exit(int code);
extern uint64_t akuma_uptime(void);  /* returns microseconds */
extern void akuma_print(const char *s, size_t len);
extern int akuma_open(const char *path, size_t path_len, int flags);
extern int akuma_close(int fd);
extern int akuma_read(int fd, void *buf, size_t count);
extern int akuma_write_fd(int fd, const void *buf, size_t count);
extern int akuma_lseek(int fd, long offset, int whence);
extern int akuma_fstat_size(int fd);
extern int akuma_mkdir(const char *path, size_t path_len);

/* ========================================================================= */
/* errno                                                                     */
/* ========================================================================= */

int errno = 0;

/* ========================================================================= */
/* Memory functions                                                          */
/* ========================================================================= */

void *memset(void *s, int c, size_t n) {
    unsigned char *p = (unsigned char *)s;
    while (n--) *p++ = (unsigned char)c;
    return s;
}

void *memcpy(void *dest, const void *src, size_t n) {
    unsigned char *d = (unsigned char *)dest;
    const unsigned char *s = (const unsigned char *)src;
    while (n--) *d++ = *s++;
    return dest;
}

void *memmove(void *dest, const void *src, size_t n) {
    unsigned char *d = (unsigned char *)dest;
    const unsigned char *s = (const unsigned char *)src;
    if (d < s) {
        while (n--) *d++ = *s++;
    } else if (d > s) {
        d += n; s += n;
        while (n--) *--d = *--s;
    }
    return dest;
}

int memcmp(const void *s1, const void *s2, size_t n) {
    const unsigned char *p1 = (const unsigned char *)s1;
    const unsigned char *p2 = (const unsigned char *)s2;
    while (n--) {
        if (*p1 != *p2) return *p1 - *p2;
        p1++; p2++;
    }
    return 0;
}

/* Memory allocation: malloc/free/realloc/calloc are provided by Rust main.rs */
/* The C stubs layer calls akuma_malloc/akuma_free/akuma_realloc which       */
/* delegate to those Rust implementations.                                   */

/* ========================================================================= */
/* String functions                                                          */
/* ========================================================================= */

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) p++;
    return p - s;
}

int strcmp(const char *s1, const char *s2) {
    while (*s1 && *s1 == *s2) { s1++; s2++; }
    return (unsigned char)*s1 - (unsigned char)*s2;
}

int strncmp(const char *s1, const char *s2, size_t n) {
    while (n && *s1 && *s1 == *s2) { s1++; s2++; n--; }
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

char *strncat(char *dest, const char *src, size_t n) {
    char *d = dest;
    while (*d) d++;
    while (n-- && *src) *d++ = *src++;
    *d = '\0';
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
    size_t nl = strlen(needle);
    if (nl == 0) return (char *)haystack;
    while (*haystack) {
        if (strncmp(haystack, needle, nl) == 0) return (char *)haystack;
        haystack++;
    }
    return NULL;
}

char *strdup(const char *s) {
    size_t len = strlen(s) + 1;
    char *d = (char *)malloc(len);
    if (d) memcpy(d, s, len);
    return d;
}

size_t strspn(const char *s, const char *accept) {
    const char *p = s;
    while (*p) {
        const char *a = accept;
        int found = 0;
        while (*a) { if (*p == *a) { found = 1; break; } a++; }
        if (!found) break;
        p++;
    }
    return p - s;
}

size_t strcspn(const char *s, const char *reject) {
    const char *p = s;
    while (*p) {
        const char *r = reject;
        while (*r) { if (*p == *r) return p - s; r++; }
        p++;
    }
    return p - s;
}

int strcasecmp(const char *s1, const char *s2) {
    while (*s1 && *s2) {
        int c1 = (*s1 >= 'A' && *s1 <= 'Z') ? *s1 + 32 : *s1;
        int c2 = (*s2 >= 'A' && *s2 <= 'Z') ? *s2 + 32 : *s2;
        if (c1 != c2) return c1 - c2;
        s1++; s2++;
    }
    return (unsigned char)*s1 - (unsigned char)*s2;
}

int strncasecmp(const char *s1, const char *s2, size_t n) {
    while (n && *s1 && *s2) {
        int c1 = (*s1 >= 'A' && *s1 <= 'Z') ? *s1 + 32 : *s1;
        int c2 = (*s2 >= 'A' && *s2 <= 'Z') ? *s2 + 32 : *s2;
        if (c1 != c2) return c1 - c2;
        s1++; s2++; n--;
    }
    if (n == 0) return 0;
    return (unsigned char)*s1 - (unsigned char)*s2;
}

char *strerror(int errnum) {
    (void)errnum;
    return "error";
}

/* ========================================================================= */
/* ctype functions                                                           */
/* ========================================================================= */

int isalpha(int c) { return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'); }
int isdigit(int c) { return c >= '0' && c <= '9'; }
int isalnum(int c) { return isalpha(c) || isdigit(c); }
int isspace(int c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v'; }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isprint(int c) { return c >= 0x20 && c <= 0x7e; }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int toupper(int c) { return (c >= 'a' && c <= 'z') ? c - 32 : c; }
int tolower(int c) { return (c >= 'A' && c <= 'Z') ? c + 32 : c; }

/* ========================================================================= */
/* Number parsing                                                            */
/* ========================================================================= */

long strtol(const char *nptr, char **endptr, int base) {
    long result = 0;
    int negative = 0;
    while (isspace(*nptr)) nptr++;
    if (*nptr == '-') { negative = 1; nptr++; }
    else if (*nptr == '+') { nptr++; }

    if (base == 0) {
        if (*nptr == '0' && (nptr[1] == 'x' || nptr[1] == 'X')) { base = 16; nptr += 2; }
        else if (*nptr == '0') { base = 8; nptr++; }
        else { base = 10; }
    } else if (base == 16 && *nptr == '0' && (nptr[1] == 'x' || nptr[1] == 'X')) {
        nptr += 2;
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

int atoi(const char *nptr) { return (int)strtol(nptr, NULL, 10); }
long atol(const char *nptr) { return strtol(nptr, NULL, 10); }

double strtod(const char *nptr, char **endptr) {
    double result = 0.0;
    int negative = 0;
    while (isspace(*nptr)) nptr++;
    if (*nptr == '-') { negative = 1; nptr++; }
    else if (*nptr == '+') { nptr++; }

    while (*nptr >= '0' && *nptr <= '9') {
        result = result * 10.0 + (*nptr - '0');
        nptr++;
    }
    if (*nptr == '.') {
        double frac = 0.1;
        nptr++;
        while (*nptr >= '0' && *nptr <= '9') {
            result += (*nptr - '0') * frac;
            frac *= 0.1;
            nptr++;
        }
    }
    if (endptr) *endptr = (char *)nptr;
    return negative ? -result : result;
}

double atof(const char *nptr) { return strtod(nptr, NULL); }

/* ========================================================================= */
/* Math functions (using compiler builtins)                                  */
/* ========================================================================= */

double ceil(double x)  { return __builtin_ceil(x); }
double floor(double x) { return __builtin_floor(x); }
double sqrt(double x)  { return __builtin_sqrt(x); }
double fabs(double x)  { return __builtin_fabs(x); }
double sin(double x)   { return __builtin_sin(x); }
double cos(double x)   { return __builtin_cos(x); }
double tan(double x)   { return __builtin_tan(x); }
double atan(double x)  { return __builtin_atan(x); }
double atan2(double y, double x) { return __builtin_atan2(y, x); }
double log(double x)   { return __builtin_log(x); }
double log2(double x)  { return __builtin_log2(x); }
double pow(double x, double y) { return __builtin_pow(x, y); }
double fmod(double x, double y) { return __builtin_fmod(x, y); }
double round(double x) { return __builtin_round(x); }
float floorf(float x)  { return __builtin_floorf(x); }
float ceilf(float x)   { return __builtin_ceilf(x); }
float sqrtf(float x)   { return __builtin_sqrtf(x); }
float fabsf(float x)   { return __builtin_fabsf(x); }

int isnan(double x) { return __builtin_isnan(x); }
int isinf(double x) { return __builtin_isinf(x); }

/* ========================================================================= */
/* printf / snprintf family (simplified implementation)                      */
/* ========================================================================= */

static int format_int(char *buf, size_t size, long long val, int base, int is_unsigned, int width, int zero_pad, int precision) {
    char tmp[24];
    int i = 0, neg = 0;
    unsigned long long uval;

    if (!is_unsigned && val < 0) { neg = 1; uval = (unsigned long long)(-val); }
    else { uval = (unsigned long long)val; }

    if (uval == 0) { tmp[i++] = '0'; }
    else {
        while (uval > 0) {
            int d = uval % base;
            tmp[i++] = d < 10 ? '0' + d : 'a' + d - 10;
            uval /= base;
        }
    }

    /* precision for integers: minimum number of digits (zero-padded) */
    int digit_pad = (precision > i) ? precision - i : 0;

    int total = i + digit_pad + neg;
    int pad = (width > total) ? width - total : 0;
    int written = 0;

    /* When precision is specified, width padding uses spaces (not zeros) */
    int use_zero = (precision >= 0) ? 0 : zero_pad;

    if (neg && use_zero) { if (written < (int)size - 1) buf[written] = '-'; written++; }
    if (use_zero) { for (int p = 0; p < pad; p++) { if (written < (int)size - 1) buf[written] = '0'; written++; } }
    else { for (int p = 0; p < pad; p++) { if (written < (int)size - 1) buf[written] = ' '; written++; } }
    if (neg && !use_zero) { if (written < (int)size - 1) buf[written] = '-'; written++; }

    /* Zero-pad for precision */
    for (int p = 0; p < digit_pad; p++) {
        if (written < (int)size - 1) buf[written] = '0';
        written++;
    }

    for (int j = i - 1; j >= 0; j--) {
        if (written < (int)size - 1) buf[written] = tmp[j];
        written++;
    }
    return written;
}

int vsnprintf(char *str, size_t size, const char *format, va_list ap) {
    size_t pos = 0;
    if (size == 0) return 0;

    while (*format) {
        if (*format != '%') {
            if (pos < size - 1) str[pos] = *format;
            pos++; format++;
            continue;
        }
        format++; /* skip '%' */

        /* Flags */
        int zero_pad = 0, left_align = 0;
        while (*format == '0' || *format == '-' || *format == '+' || *format == ' ') {
            if (*format == '0') zero_pad = 1;
            if (*format == '-') left_align = 1;
            format++;
        }
        (void)left_align;

        /* Width */
        int width = 0;
        if (*format == '*') { width = va_arg(ap, int); format++; }
        else { while (*format >= '0' && *format <= '9') { width = width * 10 + (*format++ - '0'); } }

        /* Precision */
        int precision = -1;
        if (*format == '.') {
            format++;
            precision = 0;
            if (*format == '*') { precision = va_arg(ap, int); format++; }
            else { while (*format >= '0' && *format <= '9') { precision = precision * 10 + (*format++ - '0'); } }
        }

        /* Length modifier */
        int is_long = 0;
        if (*format == 'l') { is_long = 1; format++; if (*format == 'l') { is_long = 2; format++; } }
        else if (*format == 'h') { format++; if (*format == 'h') format++; }
        else if (*format == 'z') { is_long = 1; format++; }

        /* Conversion */
        switch (*format) {
            case 'd': case 'i': {
                long long val = is_long >= 2 ? va_arg(ap, long long) :
                                is_long ? va_arg(ap, long) : va_arg(ap, int);
                pos += format_int(str + pos, size > pos ? size - pos : 0, val, 10, 0, width, zero_pad, precision);
                break;
            }
            case 'u': {
                unsigned long long val = is_long >= 2 ? va_arg(ap, unsigned long long) :
                                         is_long ? va_arg(ap, unsigned long) : va_arg(ap, unsigned int);
                pos += format_int(str + pos, size > pos ? size - pos : 0, (long long)val, 10, 1, width, zero_pad, precision);
                break;
            }
            case 'x': case 'X': {
                unsigned long long val = is_long >= 2 ? va_arg(ap, unsigned long long) :
                                         is_long ? va_arg(ap, unsigned long) : va_arg(ap, unsigned int);
                pos += format_int(str + pos, size > pos ? size - pos : 0, (long long)val, 16, 1, width, zero_pad, precision);
                break;
            }
            case 'p': {
                void *ptr = va_arg(ap, void*);
                if (pos < size - 1) str[pos++] = '0';
                if (pos < size - 1) str[pos++] = 'x';
                pos += format_int(str + pos, size > pos ? size - pos : 0, (long long)(uintptr_t)ptr, 16, 1, 0, 0, -1);
                break;
            }
            case 's': {
                const char *s = va_arg(ap, const char*);
                if (!s) s = "(null)";
                int slen = (int)strlen(s);
                if (precision >= 0 && precision < slen) slen = precision;
                for (int i = 0; i < slen; i++) {
                    if (pos < size - 1) str[pos] = s[i];
                    pos++;
                }
                break;
            }
            case 'c': {
                int c = va_arg(ap, int);
                if (pos < size - 1) str[pos] = (char)c;
                pos++;
                break;
            }
            case '%':
                if (pos < size - 1) str[pos] = '%';
                pos++;
                break;
            default:
                /* Unknown format - just output the character */
                if (pos < size - 1) str[pos] = *format;
                pos++;
                break;
        }
        if (*format) format++;
    }

    if (size > 0) str[pos < size ? pos : size - 1] = '\0';
    return (int)pos;
}

int snprintf(char *str, size_t size, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int ret = vsnprintf(str, size, format, ap);
    va_end(ap);
    return ret;
}

int vsprintf(char *str, const char *format, va_list ap) {
    return vsnprintf(str, 65536, format, ap);
}

int sprintf(char *str, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int ret = vsprintf(str, format, ap);
    va_end(ap);
    return ret;
}

int vfprintf(FILE *stream, const char *format, va_list ap) {
    char buf[512];
    int len = vsnprintf(buf, sizeof(buf), format, ap);
    akuma_print(buf, len < (int)sizeof(buf) ? len : (int)sizeof(buf) - 1);
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
    int ret = vfprintf(NULL, format, ap);
    va_end(ap);
    return ret;
}

int vprintf(const char *format, va_list ap) {
    return vfprintf(NULL, format, ap);
}

int sscanf(const char *str, const char *format, ...) {
    (void)str; (void)format;
    return 0;
}

/* ========================================================================= */
/* Character I/O                                                             */
/* ========================================================================= */

int putchar(int c) {
    char ch = (char)c;
    akuma_print(&ch, 1);
    return c;
}

int puts(const char *s) {
    akuma_print(s, strlen(s));
    akuma_print("\n", 1);
    return 0;
}

int fputc(int c, FILE *stream) { (void)stream; return putchar(c); }
int fputs(const char *s, FILE *stream) { (void)stream; akuma_print(s, strlen(s)); return 0; }

/* ========================================================================= */
/* FILE I/O (wraps Akuma syscalls)                                           */
/* ========================================================================= */

/* We use a simple FILE pool since DOOM doesn't open many files */
#define MAX_FILES 16

typedef struct {
    int fd;
    int error;
    int eof_flag;
    long position;
    int in_use;
} DoomFile;

static DoomFile file_pool[MAX_FILES];

/* Standard streams */
static DoomFile stdin_file  = { .fd = 0, .in_use = 1 };
static DoomFile stdout_file = { .fd = 1, .in_use = 1 };
static DoomFile stderr_file = { .fd = 2, .in_use = 1 };

/* These are declared in stdio.h */
FILE *stdin  = (FILE *)&stdin_file;
FILE *stdout = (FILE *)&stdout_file;
FILE *stderr = (FILE *)&stderr_file;

static DoomFile *alloc_file(void) {
    for (int i = 0; i < MAX_FILES; i++) {
        if (!file_pool[i].in_use) {
            memset(&file_pool[i], 0, sizeof(DoomFile));
            file_pool[i].in_use = 1;
            return &file_pool[i];
        }
    }
    return NULL;
}

FILE *fopen(const char *pathname, const char *mode) {
    if (!pathname || !mode) return NULL;

    int flags = 0; /* O_RDONLY */
    if (mode[0] == 'r') {
        flags = 0; /* O_RDONLY */
        if (mode[1] == '+') flags = 2; /* O_RDWR */
    } else if (mode[0] == 'w') {
        flags = 1 | 0x0040 | 0x0200; /* O_WRONLY | O_CREAT | O_TRUNC */
    } else if (mode[0] == 'a') {
        flags = 1 | 0x0040 | 0x0400; /* O_WRONLY | O_CREAT | O_APPEND */
    }

    int fd = akuma_open(pathname, strlen(pathname), flags);
    if (fd < 0) return NULL;

    DoomFile *f = alloc_file();
    if (!f) { akuma_close(fd); return NULL; }

    f->fd = fd;
    f->position = 0;
    return (FILE *)f;
}

int fclose(FILE *stream) {
    if (!stream) return -1;
    DoomFile *f = (DoomFile *)stream;
    akuma_close(f->fd);
    f->in_use = 0;
    return 0;
}

size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream) {
    if (!stream || !ptr || size == 0 || nmemb == 0) return 0;
    DoomFile *f = (DoomFile *)stream;
    size_t total = size * nmemb;
    size_t done = 0;
    while (done < total) {
        size_t chunk = total - done;
        if (chunk > 32768) chunk = 32768;
        int got = akuma_read(f->fd, (char *)ptr + done, chunk);
        if (got <= 0) {
            if (done == 0) f->eof_flag = 1;
            break;
        }
        done += got;
        f->position += got;
    }
    return done / size;
}

size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream) {
    if (!stream || !ptr || size == 0 || nmemb == 0) return 0;
    DoomFile *f = (DoomFile *)stream;
    size_t total = size * nmemb;
    int wrote = akuma_write_fd(f->fd, ptr, total);
    if (wrote <= 0) return 0;
    f->position += wrote;
    return wrote / size;
}

int fseek(FILE *stream, long offset, int whence) {
    if (!stream) return -1;
    DoomFile *f = (DoomFile *)stream;
    int result = akuma_lseek(f->fd, offset, whence);
    if (result < 0) return -1;
    f->position = result;
    f->eof_flag = 0;
    return 0;
}

long ftell(FILE *stream) {
    if (!stream) return -1;
    DoomFile *f = (DoomFile *)stream;
    return f->position;
}

void rewind(FILE *stream) { fseek(stream, 0, 0); }
int feof(FILE *stream) { return stream ? ((DoomFile *)stream)->eof_flag : 1; }
int ferror(FILE *stream) { return stream ? ((DoomFile *)stream)->error : 1; }
void clearerr(FILE *stream) { if (stream) { ((DoomFile *)stream)->error = 0; ((DoomFile *)stream)->eof_flag = 0; } }
int fflush(FILE *stream) { return 0; }
int fgetc(FILE *stream) { unsigned char c; return fread(&c, 1, 1, stream) == 1 ? c : -1; }
int getc(FILE *stream) { return fgetc(stream); }
int getchar(void) { return fgetc(stdin); }
int ungetc(int c, FILE *stream) { (void)c; (void)stream; return -1; /* not implemented */ }

char *fgets(char *s, int size, FILE *stream) {
    if (!s || size <= 0 || !stream) return NULL;
    int i = 0;
    while (i < size - 1) {
        int c = fgetc(stream);
        if (c < 0) break;
        s[i++] = (char)c;
        if (c == '\n') break;
    }
    if (i == 0) return NULL;
    s[i] = '\0';
    return s;
}

int remove(const char *pathname) { (void)pathname; return -1; }
int rename(const char *oldpath, const char *newpath) { (void)oldpath; (void)newpath; return -1; }

/* ========================================================================= */
/* sys/stat                                                                  */
/* ========================================================================= */

int mkdir(const char *path, int mode) {
    (void)mode;
    return akuma_mkdir(path, strlen(path));
}

/* ========================================================================= */
/* qsort (simple shell sort)                                                 */
/* ========================================================================= */

void qsort(void *base, size_t nmemb, size_t size,
           int (*compar)(const void *, const void *)) {
    unsigned char *b = (unsigned char *)base;
    unsigned char *tmp = (unsigned char *)malloc(size);
    if (!tmp) return;

    for (size_t gap = nmemb / 2; gap > 0; gap /= 2) {
        for (size_t i = gap; i < nmemb; i++) {
            memcpy(tmp, b + i * size, size);
            size_t j = i;
            while (j >= gap && compar(b + (j - gap) * size, tmp) > 0) {
                memcpy(b + j * size, b + (j - gap) * size, size);
                j -= gap;
            }
            memcpy(b + j * size, tmp, size);
        }
    }
    free(tmp);
}

/* ========================================================================= */
/* Misc                                                                      */
/* ========================================================================= */

void abort(void) { akuma_exit(134); }
void exit(int status) { akuma_exit(status); }
void _exit(int status) { akuma_exit(status); }

char *getenv(const char *name) { (void)name; return NULL; }
int abs(int x) { return x < 0 ? -x : x; }
long labs(long x) { return x < 0 ? -x : x; }

static unsigned int rand_state = 1;
int rand(void) {
    rand_state = rand_state * 1103515245 + 12345;
    return (int)((rand_state >> 16) & 0x7fff);
}
void srand(unsigned int seed) { rand_state = seed; }

/* ========================================================================= */
/* unistd stubs                                                              */
/* ========================================================================= */

unsigned int sleep(unsigned int seconds) {
    /* Use nanosleep via uptime busy-wait (or just return) */
    (void)seconds;
    return 0;
}

int usleep(unsigned long usec) { (void)usec; return 0; }

int access(const char *pathname, int mode) {
    (void)pathname; (void)mode;
    return -1; /* not accessible */
}

int isatty(int fd) { return fd <= 2 ? 1 : 0; }

char *getcwd(char *buf, size_t size) {
    if (buf && size > 1) {
        buf[0] = '/';
        buf[1] = '\0';
    }
    return buf;
}

int chdir(const char *path) { (void)path; return 0; }

long sysconf(int name) { (void)name; return 4096; }

int system(const char *command) { (void)command; return -1; }
