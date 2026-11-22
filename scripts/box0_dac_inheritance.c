/* Run after an empty capset, with /tmp/box0-own (uid 0) and
 * /tmp/box0-other (uid 123) both mode 0600. Build twice with clang
 * --target=<arch>-linux-gnu -fuse-ld=lld -nostdlib -static
 * -fno-stack-protector -O2, defining EXEC_PHASE for the second binary,
 * installed as /tmp/box0-dac-exec. AArch64 needs asm-generic UAPI headers.
 * Run the first binary through bwrap --bind / / --dev /dev --proc /proc
 * --cap-drop ALL -- /tmp/box0-dac. A zero exit means all checks passed. */
#if defined(__x86_64__)
#include <asm/unistd.h>
#else
#include <asm-generic/unistd.h>
#endif
static long sc(long n, long a, long b, long c, long d) {
#if defined(__x86_64__)
    register long r10 asm("r10") = d; long ret;
    asm volatile("syscall" : "=a"(ret) : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory"); return ret;
#else
    register long x8 asm("x8") = n, x0 asm("x0") = a, x1 asm("x1") = b, x2 asm("x2") = c, x3 asm("x3") = d;
    asm volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory"); return x0;
#endif
}
#define P(x) ((long)(x))
static int failures;
static void check(int ok, const char *s) {
    const char *prefix = ok ? "PASS " : "FAIL "; unsigned n = 0;
    while (prefix[n]) n++; sc(__NR_write, 1, P(prefix), n, 0);
    n = 0; while (s[n]) n++; sc(__NR_write, 1, P(s), n, 0);
    sc(__NR_write, 1, P("\n"), 1, 0); if (!ok) failures++;
}
static void permissions(void) {
    long fd = sc(__NR_openat, -100, P("/tmp/box0-own"), 0, 0);
    check(fd >= 0, "dropped uid 0 reads own file");
    if (fd >= 0) { check(sc(__NR_fchmod, fd, 0600, 0, 0) == 0, "dropped uid 0 chmods own file"); sc(__NR_close, fd, 0, 0, 0); }
    check(sc(__NR_openat, -100, P("/tmp/box0-other"), 0, 0) == -13, "dropped uid 0 cannot read another owner");
    check(sc(__NR_fchmodat, -100, P("/tmp/box0-other"), 0777, 0) == -1, "dropped uid 0 cannot chmod another owner");
}
void _start(void) {
    permissions();
#ifndef EXEC_PHASE
    long child = sc(__NR_clone, 17, 0, 0, 0);
    if (child == 0) {
        permissions();
        static const char *argv[] = {"/tmp/box0-dac-exec", 0};
        static const char *env[] = {0};
        if (!failures) sc(__NR_execve, P(argv[0]), P(argv), P(env), 0);
        sc(__NR_exit, 1, 0, 0, 0);
    }
    int status = -1;
    check(child > 0 && sc(__NR_wait4, child, P(&status), 0, 0) == child && status == 0, "fork and exec retain capability drop");
#endif
    sc(__NR_exit, failures != 0, 0, 0, 0); for (;;) {}
}
