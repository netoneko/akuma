/*
 * pwv2probe: `pwritev2` with RWF_NOAPPEND, which is how newer musl (Alpine
 * 3.24, 1.2.6) implements pwrite(2) / what SQLite and Chromium's file writes
 * reach. Akuma answered EOPNOTSUPP for every nonzero RWF_* flag, so 990 writes
 * per headless Chromium run failed, and the browser's font service (which
 * copies font data through such a write) handed the renderer nothing: pages
 * rendered with no system-font text (ryzen, 2026-10-08).
 *
 * Scored (Linux passes all): flags 0 writes at the offset; RWF_NOAPPEND writes
 * at the offset; offset -1 + RWF_NOAPPEND uses the file position; an unknown
 * flag bit is EOPNOTSUPP; libc pwrite lands at its offset. Printed, not scored
 * (Linux 6.17 on overlayfs, measured 2026-10-08): RWF_NOAPPEND on an O_APPEND
 * fd still appends, and a preadv2 with the flag succeeds (ignored).
 * Argument: the directory to create the file in.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef RWF_NOAPPEND
#define RWF_NOAPPEND 0x20
#endif

static int fails;
static void check(const char *what, int ok) {
    printf("pwv2probe: %s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}
static long pw2(int fd, const char *s, long off, int flags) {
    struct iovec iov = { (void *)s, strlen(s) };
    return syscall(SYS_pwritev2, fd, &iov, 1, off, 0 /* pos_h */, flags);
}
static void at(int fd, long off, char *out, int n) {
    memset(out, 0, n + 1);
    if (pread(fd, out, n, off) < 0) out[0] = 0;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    char p[128], b[32];
    snprintf(p, sizeof p, "%s/.pwv2probe.%d", dir, getpid());
    int fd = open(p, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) { perror("open"); return 1; }
    unlink(p);

    check("pwritev2 flags=0 at 0 writes 4", pw2(fd, "AAAA", 0, 0) == 4);
    check("pwritev2 RWF_NOAPPEND at 10 writes 4", pw2(fd, "BBBB", 10, RWF_NOAPPEND) == 4);
    at(fd, 10, b, 4);
    check("  ... and the bytes are at offset 10", strcmp(b, "BBBB") == 0);

    int ap = open("/proc/self/exe", O_RDONLY); /* keep fd numbers busy; ignored */
    if (ap >= 0) close(ap);
    snprintf(p, sizeof p, "%s/.pwv2probe-a.%d", dir, getpid());
    int fa = open(p, O_RDWR | O_CREAT | O_EXCL | O_APPEND, 0600);
    unlink(p);
    check("open O_APPEND", fa >= 0);
    check("write 8 bytes to the O_APPEND file", write(fa, "01234567", 8) == 8);
    check("pwritev2 RWF_NOAPPEND on an O_APPEND fd at 2", pw2(fa, "XY", 2, RWF_NOAPPEND) == 2);
    at(fa, 0, b, 12);
    printf("pwv2probe: (info) O_APPEND file reads back \"%s\"\n", b);
    /* Linux (6.17, overlayfs) appends here: "01234567XY". Not scored either way. */

    lseek(fd, 20, SEEK_SET);
    check("pwritev2 offset -1 + RWF_NOAPPEND writes", pw2(fd, "CC", -1, RWF_NOAPPEND) == 2);
    at(fd, 20, b, 2);
    check("  ... at the file position (20)", strcmp(b, "CC") == 0);

    struct iovec rv = { b, 2 };
    errno = 0;
    long r = syscall(SYS_preadv2, fd, &rv, 1, 0L, 0, RWF_NOAPPEND);
    printf("pwv2probe: (info) preadv2 RWF_NOAPPEND -> %ld errno %d\n", r, r < 0 ? errno : 0);
    errno = 0;
    r = pw2(fd, "Z", 0, 0x40000000);
    check("an unknown flag bit is EOPNOTSUPP", r == -1 && errno == EOPNOTSUPP);

    check("libc pwrite at 30 writes 3", pwrite(fd, "DDD", 3, 30) == 3);
    at(fd, 30, b, 3);
    check("  ... at offset 30", strcmp(b, "DDD") == 0);

    lseek(fd, 40, SEEK_SET);
    check("plain write() on the unlinked file writes 3", write(fd, "EEE", 3) == 3);
    at(fd, 40, b, 3);
    check("  ... and reads back at 40", strcmp(b, "EEE") == 0);

    printf("pwv2probe: %s\n", fails ? "FAILED" : "all ok");
    return fails != 0;
}
