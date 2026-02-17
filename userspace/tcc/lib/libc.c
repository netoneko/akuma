#include <stddef.h>
#include <stdarg.h>

void exit(int status);
long write(int fd, const void *buf, size_t count);

// Syscall wrapper
static inline long syscall(long num, long a0, long a1, long a2, long a3, long a4, long a5) {
    long ret;
    asm volatile(
        "mov x8, %1\n" // Use \n for newlines within the string
        "mov x0, %2\n"
        "mov x1, %3\n"
        "mov x2, %4\n"
        "mov x3, %5\n"
        "mov x4, %6\n"
        "mov x5, %7\n"
        "svc #0\n" // Supervisor Call for Akuma/AArch64
        "mov %0, x0\n"
        : "=r"(ret)
        : "r"(num), "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5)
        : "x0", "x1", "x2", "x3", "x4", "x5", "x8", "memory"
    );
    return ret;
}

// Minimal libc stubs
void exit(int status) {
    syscall(93, status, 0, 0, 0, 0, 0); // Akuma's exit syscall number 93
    while (1);
}

// write(2)
long write(int fd, const void *buf, size_t count) {
    return syscall(64, fd, (long)buf, count, 0, 0, 0); // Akuma's write syscall number 64
}

// read(2)
long read(int fd, void *buf, size_t count) {
    return syscall(63, fd, (long)buf, count, 0, 0, 0); // Akuma's read syscall number 63
}

// open(2)
long open(const char *pathname, int flags, int mode) {
    return syscall(56, (long)pathname, flags, mode, 0, 0, 0); // Akuma's open syscall number 56
}

// close(2)
long close(int fd) {
    return syscall(57, fd, 0, 0, 0, 0, 0); // Akuma's close syscall number 57
}

// lseek(2)
long lseek(int fd, long offset, int whence) {
    return syscall(62, fd, offset, whence, 0, 0, 0); // Akuma's lseek syscall number 62
}

// Minimal implementation of puts
int puts(const char *s) {
    long bytes_written = 0;
    const char *ptr = s;
    while (*ptr != '\0') {
        ptr++;
    }
    bytes_written += write(1, s, ptr - s); // Write to stdout
    bytes_written += write(1, "\n", 1); // Write newline
    return (int)bytes_written;
}

// Minimal implementation of strlen
size_t strlen(const char *s) {
    size_t len = 0;
    while (s[len] != '\0') {
        len++;
    }
    return len;
}

// Minimal implementation of memcpy
void *memcpy(void *dest, const void *src, size_t n) {
    unsigned char *d = dest;
    const unsigned char *s = src;
    for (size_t i = 0; i < n; i++) {
        d[i] = s[i];
    }
    return dest;
}

// Minimal implementation of memset
void *memset(void *s, int c, size_t n) {
    unsigned char *p = s;
    for (size_t i = 0; i < n; i++) {
        p[i] = (unsigned char)c;
    }
    return s;
}

// Minimal implementation of putchar
int putchar(int c) {
    char ch = (char)c;
    write(1, &ch, 1);
    return c;
}

// Minimal implementation of printf (very basic, no formatting)
int printf(const char *format, ...) {
    // This is extremely minimal and only handles printing a string directly
    // For proper printf, a more complex implementation is needed.
    // For TCC's minimal requirement, this might suffice for "Hello World".
    va_list args;
    va_start(args, format);
    const char *s = va_arg(args, const char *);
    va_end(args);

    return (int)write(1, s, strlen(s));
}


