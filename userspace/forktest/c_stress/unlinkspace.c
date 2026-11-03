/* unlinkspace: deleting a file gives its blocks back when Linux would.
 *
 *   after_munmap   a file mapped, read, unmapped and closed, then unlinked:
 *                  its blocks are free at the unlink, in the same process.
 *   while_mapped   unlinked while still mapped: the blocks stay (the mapping
 *                  still reads the file's bytes) ...
 *   at_munmap      ... and are free once the mapping goes, with no other
 *                  unlink or create in between.
 *   at_close       unlinked while an fd is open: free once the fd closes.
 *
 * "Free" is statvfs(2)'s f_bfree back at its value before the file was
 * written (within SLACK blocks, for directory and log churn elsewhere).
 *
 * Usage: unlinkspace [DIR]   (default /root; DIR must be on a disk filesystem)
 * Prints PASS/FAIL per check and "unlinkspace: N/N passed".
 */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/statvfs.h>
#include <unistd.h>

#define SIZE (16 << 20)
#define SLACK 64

static const char *dir;
static int passed, total;

static long free_blocks(void) {
    struct statvfs s;
    if (statvfs(dir, &s) != 0)
        return -1;
    return (long)s.f_bfree;
}

static void check(const char *name, int ok, long base, long now) {
    total++;
    passed += ok != 0;
    printf("%s %s (blocks in use vs start: %ld)\n", ok ? "PASS" : "FAIL", name, base - now);
    fflush(stdout);
}

static int back(long base, long now) {
    return base - now <= SLACK;
}

/* Write SIZE bytes to a new file; its fd, or -1. */
static int make(const char *path) {
    static char buf[1 << 20];
    memset(buf, 7, sizeof buf);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        return -1;
    for (int i = 0; i < SIZE; i += (int)sizeof buf)
        if (write(fd, buf, sizeof buf) != (ssize_t)sizeof buf)
            return -1;
    fsync(fd);
    return fd;
}

static long touch_all(const char *m) {
    volatile long s = 0;
    for (int i = 0; i < SIZE; i += 4096)
        s += m[i];
    return s;
}

int main(int argc, char **argv) {
    dir = argc > 1 ? argv[1] : "/root";
    char path[512];
    snprintf(path, sizeof path, "%s/unlinkspace.dat", dir);

    /* after_munmap */
    long base = free_blocks();
    int fd = make(path);
    char *m = fd < 0 ? MAP_FAILED : mmap(0, SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) {
        printf("FAIL setup: %s\n", fd < 0 ? "write" : "mmap");
        return 1;
    }
    touch_all(m);
    munmap(m, SIZE);
    close(fd);
    unlink(path);
    long now = free_blocks();
    check("after_munmap", back(base, now), base, now);

    /* while_mapped, at_munmap */
    base = free_blocks();
    fd = make(path);
    m = fd < 0 ? MAP_FAILED : mmap(0, SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) {
        printf("FAIL setup 2\n");
        return 1;
    }
    close(fd);
    unlink(path);
    now = free_blocks();
    /* Untouched pages still read the file: the mapping holds it. */
    check("while_mapped", base - now > SIZE / 4096 / 2 && m[SIZE - 1] == 7, base, now);
    munmap(m, SIZE);
    now = free_blocks();
    check("at_munmap", back(base, now), base, now);

    /* at_close */
    base = free_blocks();
    fd = make(path);
    if (fd < 0) {
        printf("FAIL setup 3\n");
        return 1;
    }
    unlink(path);
    char c = 0;
    int readable = pread(fd, &c, 1, SIZE - 1) == 1 && c == 7;
    close(fd);
    now = free_blocks();
    check("at_close", readable && back(base, now), base, now);

    printf("unlinkspace: %d/%d passed\n", passed, total);
    return passed == total ? 0 : 1;
}
