/*
 * fpcoherent - a file changed after a page of it was cached must read as changed.
 *
 * The shared file-page cache keeps one frame per (inode, offset) for read-only
 * file mappings, across processes and after every mapping is gone. Each probe
 * here gets a page into the cache (a child maps the file read-only and touches
 * it, then exits), changes the file one way, and maps it again: the new mapping
 * must read the new bytes. A stale cached page is a silent wrong read - for
 * rustc, a miscompile from an .rmeta cargo rewrote.
 *
 * Ways in: write(2), pwrite(2) on an unlinked file through its descriptor,
 * ftruncate(2) shrinking then growing, rename(2) over the name, and a
 * MAP_SHARED write-back (msync). docs/archive/TAOXICHUAN_LAZY_MMAP.md
 *
 * Prints PASS/FAIL per probe; exit 0 only when all pass.
 *
 *   x86_64-linux-musl-gcc -static -O2 -Wall -Wextra -o fpcoherent fpcoherent.c
 */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGES 4
#define LEN (PAGES * 4096)

static int fails;

static void report(const char *name, int ok, const char *detail)
{
    printf("%s %s%s%s\n", ok ? "PASS" : "FAIL", name, detail ? ": " : "", detail ? detail : "");
    if (!ok)
        fails++;
}

static int fill(int fd, char c)
{
    char buf[LEN];
    memset(buf, c, sizeof buf);
    return pwrite(fd, buf, sizeof buf, 0) == (ssize_t)sizeof buf ? 0 : -1;
}

/* Map `fd` read-only and sum the bytes of every page in a child, so the pages
 * land in the cache and the mapping is gone before the parent changes the file. */
static void warm(int fd)
{
    pid_t p = fork();
    if (p == 0) {
        volatile const char *m = mmap(NULL, LEN, PROT_READ, MAP_PRIVATE, fd, 0);
        if (m == MAP_FAILED)
            _exit(1);
        unsigned s = 0;
        for (int i = 0; i < LEN; i += 4096)
            s += m[i];
        _exit(s == 0xFFFF ? 2 : 0);
    }
    waitpid(p, NULL, 0);
}

/* Does a fresh read-only mapping of `fd` hold `c` in every page? */
static int reads_as(int fd, char c, char *got)
{
    const char *m = mmap(NULL, LEN, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) {
        *got = '?';
        return 0;
    }
    int ok = 1;
    for (int i = 0; i < LEN; i += 4096)
        if (m[i] != c || m[i + 4095] != c) {
            *got = m[i];
            ok = 0;
        }
    munmap((void *)m, LEN);
    if (ok)
        *got = c;
    return ok;
}

static void check(const char *name, int fd, char want)
{
    char got, detail[64];
    int ok = reads_as(fd, want, &got);
    snprintf(detail, sizeof detail, "want '%c' got '%c'", want, got);
    report(name, ok, detail);
}

int main(void)
{
    const char *a = "/tmp/fpcoh.a", *b = "/tmp/fpcoh.b";
    unlink(a);
    unlink(b);

    /* write(2) through the name. */
    int fd = open(a, O_RDWR | O_CREAT | O_TRUNC, 0644);
    fill(fd, 'A');
    warm(fd);
    check("cached, unchanged (control)", fd, 'A');
    fill(fd, 'B');
    check("write(2) after caching", fd, 'B');

    /* pwrite(2) to an unlinked file through its descriptor: no name to invalidate by. */
    warm(fd);
    unlink(a);
    fill(fd, 'C');
    check("pwrite(2) on an unlinked file", fd, 'C');
    close(fd);

    /* ftruncate shrink then grow: the cached bytes must not come back. */
    fd = open(a, O_RDWR | O_CREAT | O_TRUNC, 0644);
    fill(fd, 'D');
    warm(fd);
    ftruncate(fd, 0);
    ftruncate(fd, LEN);
    check("ftruncate to 0 and back", fd, '\0');
    close(fd);

    /* rename(2) over the name: a mapping by name reads the new file. */
    fd = open(a, O_RDWR | O_CREAT | O_TRUNC, 0644);
    fill(fd, 'E');
    warm(fd);
    close(fd);
    int fb = open(b, O_RDWR | O_CREAT | O_TRUNC, 0644);
    fill(fb, 'F');
    close(fb);
    rename(b, a);
    fd = open(a, O_RDONLY);
    check("rename(2) over a cached file", fd, 'F');
    close(fd);

    /* MAP_SHARED write-back. */
    fd = open(a, O_RDWR);
    warm(fd);
    char *w = mmap(NULL, LEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (w == MAP_FAILED) {
        report("MAP_SHARED write-back", 0, "mmap failed");
    } else {
        memset(w, 'G', LEN);
        msync(w, LEN, MS_SYNC);
        munmap(w, LEN);
        check("MAP_SHARED write-back", fd, 'G');
    }
    close(fd);
    unlink(a);

    printf("%s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails != 0;
}
