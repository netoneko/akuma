/* shmshare: MAP_SHARED pages are one copy for every mapper.
 *
 *   anon_fork      MAP_SHARED|MAP_ANONYMOUS: a child's store is the parent's,
 *                  on a page the parent touched before the fork and on one it
 *                  never touched.
 *   file_fork      a writable MAP_SHARED file mapping inherited by fork: the
 *                  child's store is visible to the parent without msync.
 *   file_remap     a child that maps the file itself (not inherited) sees the
 *                  parent's stores, and the parent sees the child's.
 *   write_through  pwrite(2) to the file shows in a page already mapped.
 *   writeback      after every mapping is gone, read(2) returns the stores.
 *   fresh_map      a new mapping after that reads the file, not a stale frame.
 *   mprotect_keeps mprotect(PROT_READ) then RW on a shared page keeps sharing.
 *   unaligned_off  an offset that is not a page multiple is EINVAL.
 *
 * Usage: shmshare [DIR]   (default /tmp; the file is DIR/shmshare.dat)
 * Prints PASS/FAIL per check and "shmshare: N/N passed".
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096
#define PAGES 4

static int passed, total;

static void check(const char *name, int ok) {
    total++;
    passed += ok != 0;
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
    fflush(stdout);
}

/* Run fn in a child; its exit status 0 is the answer. */
static int in_child(int (*fn)(void *), void *arg) {
    pid_t p = fork();
    if (p < 0)
        return 0;
    if (p == 0)
        _exit(fn(arg));
    int st = 0;
    if (waitpid(p, &st, 0) != p)
        return 0;
    return WIFEXITED(st) && WEXITSTATUS(st) == 0;
}

static int anon_child(void *arg) {
    volatile unsigned char *m = arg;
    if (m[0] != 0xA1)
        return 1; /* the parent's pre-fork store */
    m[1] = 0xC1;          /* a touched page */
    m[2 * PG] = 0xC2;     /* a page the parent never touched */
    return 0;
}

static const char *path;

static int file_fork_child(void *arg) {
    volatile unsigned char *m = arg;
    m[PG + 5] = 0xD1;
    return 0;
}

static int file_remap_child(void *arg) {
    (void)arg;
    int fd = open(path, O_RDWR);
    if (fd < 0)
        return 2;
    volatile unsigned char *m = mmap(0, PAGES * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (m == MAP_FAILED)
        return 3;
    if (m[7] != 0xE1)
        return 4; /* the parent's store, through a mapping of our own */
    m[3 * PG + 9] = 0xE2;
    munmap((void *)m, PAGES * PG);
    return 0;
}

static int mprotect_child(void *arg) {
    volatile unsigned char *m = arg;
    return m[11] == 0xF2 ? 0 : 1;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    static char buf[512];
    snprintf(buf, sizeof buf, "%s/shmshare.dat", dir);
    path = buf;

    /* anon_fork */
    unsigned char *a = mmap(0, PAGES * PG, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (a == MAP_FAILED) {
        check("anon_fork (mmap)", 0);
    } else {
        a[0] = 0xA1;
        int ok = in_child(anon_child, a);
        check("anon_fork", ok && a[1] == 0xC1 && a[2 * PG] == 0xC2);
        munmap(a, PAGES * PG);
    }

    /* The file: PAGES pages of a known pattern. */
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        printf("FAIL open %s: %s\n", path, strerror(errno));
        return 1;
    }
    static unsigned char pat[PAGES * PG];
    for (int i = 0; i < PAGES * PG; i++)
        pat[i] = (unsigned char)(i * 7);
    if (write(fd, pat, sizeof pat) != (ssize_t)sizeof pat) {
        printf("FAIL write: %s\n", strerror(errno));
        return 1;
    }
    unsigned char *m = mmap(0, PAGES * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) {
        printf("FAIL mmap: %s\n", strerror(errno));
        return 1;
    }

    /* file_fork */
    int ok = in_child(file_fork_child, m);
    check("file_fork", ok && m[PG + 5] == 0xD1);

    /* file_remap */
    m[7] = 0xE1;
    ok = in_child(file_remap_child, 0);
    check("file_remap", ok && m[3 * PG + 9] == 0xE2);

    /* write_through: the page is mapped and touched; pwrite lands in it. */
    (void)m[2 * PG];
    const unsigned char w[3] = {0x11, 0x22, 0x33};
    ok = pwrite(fd, w, 3, 2 * PG + 100) == 3;
    check("write_through", ok && m[2 * PG + 100] == 0x11 && m[2 * PG + 102] == 0x33);

    /* mprotect_keeps */
    ok = mprotect(m, PG, PROT_READ) == 0 && mprotect(m, PG, PROT_READ | PROT_WRITE) == 0;
    m[11] = 0xF2;
    ok = ok && in_child(mprotect_child, m);
    check("mprotect_keeps", ok);

    /* writeback */
    munmap(m, PAGES * PG);
    static unsigned char back[PAGES * PG];
    ok = pread(fd, back, sizeof back, 0) == (ssize_t)sizeof back;
    ok = ok && back[7] == 0xE1 && back[11] == 0xF2 && back[PG + 5] == 0xD1 && back[3 * PG + 9] == 0xE2 &&
         back[2 * PG + 100] == 0x11 && back[13] == pat[13];
    check("writeback", ok);

    /* fresh_map: rewrite a byte by write(2) while nothing maps the file. */
    const unsigned char z = 0x5A;
    ok = pwrite(fd, &z, 1, PG + 5) == 1;
    unsigned char *m2 = mmap(0, PAGES * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ok = ok && m2 != MAP_FAILED && m2[PG + 5] == 0x5A && m2[7] == 0xE1;
    check("fresh_map", ok);
    if (m2 != MAP_FAILED)
        munmap(m2, PAGES * PG);

    /* unaligned_off */
    void *u = mmap(0, PG, PROT_READ, MAP_SHARED, fd, 100);
    check("unaligned_off", u == MAP_FAILED && errno == EINVAL);

    close(fd);
    unlink(path);
    printf("shmshare: %d/%d passed\n", passed, total);
    return passed == total ? 0 : 1;
}
