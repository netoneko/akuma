#include <stddef.h>
#include <stdarg.h>

void exit(int status);
long write(int fd, const void *buf, size_t count);

// Syscall wrapper
static inline long syscall(long num, long a0, long a1, long a2, long a3, long a4, long a5) {
    long ret;
    asm volatile(
        "mov x8, %1\n"
        "mov x0, %2\n"
        "mov x1, %3\n"
        "mov x2, %4\n"
        "mov x3, %5\n"
        "mov x4, %6\n"
        "mov x5, %7\n"
        "svc #0\n"
        "mov %0, x0"
        : "=r"(ret)
        : "r"(num), "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5)
        : "x0", "x1", "x2", "x3", "x4", "x5", "x8", "memory"
    );
    return ret;
}

__attribute__((visibility("default")))
void exit(int status) {
    syscall(0, status, 0, 0, 0, 0, 0);
    while (1) {}
}

long write(int fd, const void *buf, size_t count) {
    return syscall(2, fd, (long)buf, count, 0, 0, 0);
}

__attribute__((visibility("default")))
int printf(const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    while (*format) {
        if (*format == '%') {
            format++;
            if (*format == 's') {
                char *s = va_arg(ap, char*);
                if (!s) s = "(null)";
                size_t len = 0;
                while (s[len]) len++;
                write(1, s, len);
            } else if (*format == 'd' || *format == 'i') {
                int val = va_arg(ap, int);
                if (val < 0) {
                    write(1, "-", 1);
                    val = -val;
                }
                char buf[12];
                int i = 0;
                if (val == 0) buf[i++] = '0';
                else {
                    while (val > 0) {
                        buf[i++] = (val % 10) + '0';
                        val /= 10;
                    }
                }
                while (i > 0) write(1, &buf[--i], 1);
            } else if (*format == 'x' || *format == 'p') {
                unsigned long val;
                if (*format == 'p') {
                    val = (unsigned long)va_arg(ap, void*);
                    write(1, "0x", 2);
                } else {
                    val = (unsigned int)va_arg(ap, unsigned int);
                }
                char buf[16];
                int i = 0;
                if (val == 0) buf[i++] = '0';
                else {
                    while (val > 0) {
                        int digit = val % 16;
                        buf[i++] = digit < 10 ? digit + '0' : digit - 10 + 'a';
                        val /= 16;
                    }
                }
                while (i > 0) write(1, &buf[--i], 1);
            } else if (*format == '%') {
                write(1, "%", 1);
            } else {
                write(1, "?", 1);
            }
            format++;
        } else {
            write(1, format, 1);
            format++;
        }
    }
    va_end(ap);
    return 0;
}
