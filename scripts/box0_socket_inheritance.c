/* Run as init inside Akuma: socket policy follows inherited descriptors.
 * Build with clang --target=<arch>-linux-gnu -fuse-ld=lld -nostdlib -static
 * -fno-stack-protector -O2; AArch64 also needs asm-generic Linux UAPI headers.
 * The guest must enable sc-private-ns and the smoltcp stack.
 */
#if defined(__x86_64__)
#include <asm/unistd.h>
#else
#include <asm-generic/unistd.h>
#endif
static long sc(long n, long a, long b, long c, long d) {
#if defined(__x86_64__)
    register long r10 asm("r10") = d;
    long ret;
    asm volatile("syscall" : "=a"(ret) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return ret;
#else
    register long x8 asm("x8") = n;
    register long x0 asm("x0") = a;
    register long x1 asm("x1") = b;
    register long x2 asm("x2") = c;
    register long x3 asm("x3") = d;
    asm volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
#endif
}
#define P(x) ((long)(x))
static const struct { short family; char name[20]; } un = { 1, "\0box0-inherit-test" };
static const struct { short family; unsigned short port; unsigned char ip[4]; char zero[8]; } ip = { 2, 0x4567, {127,0,0,1}, {0} };
static int failures;
static void check(int ok, const char *s) {
    const char *prefix = ok ? "PASS " : "FAIL ";
    unsigned n = 0;
    while (prefix[n]) n++;
    sc(__NR_write, 1, P(prefix), n, 0);
    n = 0;
    while (s[n]) n++;
    sc(__NR_write, 1, P(s), n, 0);
    sc(__NR_write, 1, P("\n"), 1, 0);
    if (!ok) failures++;
}
void _start(void) {
    long listener = sc(__NR_socket, 1, 2, 0, 0);
    long inherited_unix = sc(__NR_socket, 1, 2, 0, 0);
    long inherited_inet = sc(__NR_socket, 2, 2, 0, 0);
    check(sc(__NR_bind, listener, P(&un), 20, 0) == 0, "host abstract bind");
    long child = sc(__NR_clone, 0x20000 | 17, 0, 0, 0);
    if (child == 0) {
        long fresh_unix = sc(__NR_socket, 1, 2, 0, 0);
        long fresh_inet = sc(__NR_socket, 2, 2, 0, 0);
        check(sc(__NR_connect, inherited_unix, P(&un), 20, 0) == 0, "inherited abstract socket reaches host namespace");
        check(sc(__NR_connect, fresh_unix, P(&un), 20, 0) < 0, "new abstract socket cannot reach host namespace");
        check(sc(__NR_connect, inherited_inet, P(&ip), 16, 0) == 0, "inherited IPv4 socket retains host loopback");
        check(sc(__NR_connect, fresh_inet, P(&ip), 16, 0) == -13, "new IPv4 socket cannot reach host loopback");
        sc(__NR_exit, failures != 0, 0, 0, 0);
    }
    check(child > 0, "private namespace clone");
    if (child > 0) {
        int status = -1;
        check(sc(__NR_wait4, child, P(&status), 0, 0) == child && status == 0, "child checks passed");
    }
    sc(__NR_exit, failures != 0, 0, 0, 0);
    for (;;) {}
}
