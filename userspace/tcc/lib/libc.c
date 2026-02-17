#include <stddef.h>
#include <stdarg.h>

void exit(int status);
long write(int fd, const void *buf, size_t count);

// Syscall wrapper
static inline long syscall(long num, long a0, long a1, long a2, long a3, long a4, long a5) {
    long ret;
    asm volatile(
        "mov x8, %1
"
        "mov x0, %2
"
        "mov x1, %3
"
        "mov x2, %4
"
        "mov x3, %5
"
        "mov x4, %6
"
        "mov x5, %7
"
        "svc #0
"
        "mov %0, x0"
        : "=r"(ret)
        : "r"(num), "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5)
        : "x0", "x1", "x2", "x3", "x4", "x5", "x8", "memory"
    );
    return ret;
}

void exit(int status) {
    syscall(0, status, 0, 0, 0, 0, 0);
    while (1) {}
}

long write(int fd, const void *buf, size_t count) {
    return syscall(2, fd, (long)buf, count, 0, 0, 0);
}

int printf(const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    while (*format) {
        if (*format == '%' && *(format + 1) == 's') {
            char *s = va_arg(ap, char*);
            size_t len = 0;
            while (s[len]) len++;
            write(1, s, len);
            format += 2;
        } else {
            write(1, format, 1);
            format++;
        }
    }
    va_end(ap);
    return 0;
}
