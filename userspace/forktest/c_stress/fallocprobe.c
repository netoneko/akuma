/*
 * fallocprobe: what Chromium does to size a shared-memory file
 * (base/memory/platform_shared_memory_region_posix.cc): create the file,
 * unlink it at once, fallocate(fd, 0, 0, size), then map it MAP_SHARED.
 * Linux traces show 171 such calls per page load, all returning 0.
 *
 * Akuma amd64 before 2026-10-08 had no row for x86_64 285, so every call was
 * ENOSYS (82 per headless run) and Chromium fell back to ftruncate.
 *
 * Scored (Linux passes all): mode 0 grows the file to offset+len and reads
 * back zeros; works on an unlinked file; never shrinks; offset+len past EOF
 * grows to exactly that; a shared mapping sees writes at the far end;
 * len 0 -> EINVAL. Printed, not scored: mode FALLOC_FL_KEEP_SIZE (Akuma's
 * ext2 answers EOPNOTSUPP for any mode but 0).
 * Argument: the directory to create the file in (default /tmp).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static int fails;
static void check(const char *what, int ok) {
    printf("fallocprobe: %s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}
static long size_of(int fd) { struct stat s; return fstat(fd, &s) == 0 ? (long)s.st_size : -1; }

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    char p[128];
    snprintf(p, sizeof p, "%s/.fallocprobe.%d", dir, getpid());
    int fd = open(p, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) { perror("open"); return 1; }
    unlink(p);

    check("fallocate(0,0,131072) on an unlinked file == 0", fallocate(fd, 0, 0, 131072) == 0);
    check("size is 131072", size_of(fd) == 131072);

    static char buf[4096];
    int zeros = 1;
    for (long off = 0; off < 131072 && zeros; off += 4096) {
        memset(buf, 0xAA, sizeof buf);
        if (pread(fd, buf, sizeof buf, off) != (ssize_t)sizeof buf) { zeros = 0; break; }
        for (int i = 0; i < 4096; i++) if (buf[i]) { zeros = 0; break; }
    }
    check("the new range reads as zeros", zeros);

    check("fallocate(0,0,4096) on a larger file == 0", fallocate(fd, 0, 0, 4096) == 0);
    check("a smaller fallocate never shrinks", size_of(fd) == 131072);

    check("fallocate(0,200000,5000) == 0", fallocate(fd, 0, 200000, 5000) == 0);
    check("size is offset+len (205000)", size_of(fd) == 205000);

    char *m = mmap(0, 205000, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("MAP_SHARED of the sized file", m != MAP_FAILED);
    if (m != MAP_FAILED) {
        strcpy(m + 204000, "far-end");
        char back[16] = {0};
        /* Pinned divergence, not scored: Akuma shows a MAP_SHARED write to
         * pread only after the mapping is torn down (the ftruncate control
         * below behaves the same, so it is not fallocate's). */
        printf("fallocprobe: INFO pread sees the mapped write before munmap: %s\n",
               pread(fd, back, 8, 204000) == 8 && strcmp(back, "far-end") == 0 ? "yes" : "no");
        munmap(m, 205000);
        memset(back, 0, sizeof back);
        check("a write at the far end reads back through pread after munmap",
              pread(fd, back, 8, 204000) == 8 && strcmp(back, "far-end") == 0);
    }

    /* Control: the same map-write-pread on a file sized by ftruncate. If the
     * far-end check above fails and this one fails too, the gap is mapping/
     * pread coherence, not fallocate. */
    {
        snprintf(p, sizeof p, "%s/.fallocprobe2.%d", dir, getpid());
        int f2 = open(p, O_RDWR | O_CREAT | O_EXCL, 0600);
        unlink(p);
        int ok = 0;
        if (f2 >= 0 && ftruncate(f2, 205000) == 0) {
            char *m2 = mmap(0, 205000, PROT_READ | PROT_WRITE, MAP_SHARED, f2, 0);
            if (m2 != MAP_FAILED) {
                char back[16] = {0};
                strcpy(m2 + 204000, "far-end");
                ok = pread(f2, back, 8, 204000) == 8 && strcmp(back, "far-end") == 0;
                munmap(m2, 205000);
            }
        }
        printf("fallocprobe: INFO control (ftruncate-sized) map-write-then-pread coherent: %s\n", ok ? "yes" : "no");
        if (f2 >= 0) close(f2);
    }

    errno = 0;
    int r = fallocate(fd, 0, 0, 0);
    check("fallocate(len 0) == -1 EINVAL", r == -1 && errno == EINVAL);

    errno = 0;
    r = fallocate(fd, 0, 0, 100000);
    printf("fallocprobe: INFO KEEP_SIZE past EOF -> %d errno %d, size now %ld\n",
           fallocate(fd, FALLOC_FL_KEEP_SIZE, 300000, 4096), errno, size_of(fd));

    close(fd);
    printf("fallocprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
