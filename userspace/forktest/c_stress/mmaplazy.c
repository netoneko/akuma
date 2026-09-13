/*
 * mmaplazy.c — the correctness gate for **demand-paged** file mappings.
 *
 * `mmapsum` already asks the first question — do the bytes a mapping delivers
 * match what `read()` delivers — and that is the one a fill path gets wrong.
 * This probe asks the four that only a *lazy* fill path can get wrong, each of
 * which is silent: the mapping still works, and it delivers the wrong file's
 * bytes, or another part of this file's bytes, or zeros.
 *
 *   1. LATE PAGES. A page touched long after `mmap` returned must still hold
 *      the file's bytes. An eager kernel cannot fail this; a lazy one reads the
 *      file at fault time and has to remember where the mapping sits in it.
 *
 *   2. UNLINKED WHILE MAPPED. The fd is closed and the file removed before the
 *      pages are touched, and other files are created afterwards to invite the
 *      filesystem to reissue the inode number. A lazy mapping that does not
 *      pin its inode reads either zeros or whatever now owns that number —
 *      root cause #2 of the AArch64 self-host `rustc` ICE. `ld.so` produces the
 *      first half of this shape on every process: map, then close.
 *
 *   3. SPLIT BY MPROTECT. `mprotect(PROT_NONE)` over the middle of a mapping
 *      splits one region into three; restoring it and reading every page checks
 *      that each piece still knows its own offset in the file. Off-by-one here
 *      serves real file data from the wrong place, which no digest of a single
 *      mapping would catch. The dynamic linker makes this shape on every shared
 *      object it loads.
 *
 *   4. INHERITED BY FORK. A child faulting pages the parent never touched must
 *      get the file, not zeros — the child inherits the region record, and what
 *      that record forgets is invisible until something reads it.
 *
 * Plus two shapes that exercise **page sharing** from the outside: the same file
 * mapped twice in one process (two VAs onto one cached frame) and mapped again
 * in a child (a second address space onto it). Sharing is not observable from
 * ring 3 by design — what is observable is getting the *right bytes* through it,
 * and a reference-counting error shows up here as zeros or a crash.
 *
 * Every check compares against bytes derived from the file offset by the same
 * formula that wrote them, so there is no digest to keep in step and a wrong
 * answer names the offset it was wrong at.
 *
 * Static, musl, pure C — a crash is unambiguously the kernel's.
 * Build: <arch>-linux-musl-gcc -static -O2 -Wall -Wextra -o mmaplazy mmaplazy.c
 * Usage: mmaplazy [dir]        (default /tmp)
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096UL
/* Pages of real file data, plus a deliberately partial last page: the page that
 * straddles EOF is the one a fill path zero-fills by hand, and the one the page
 * cache must refuse to share. */
#define DATA_PAGES 24UL
#define TAIL_BYTES 1234UL
#define FILE_BYTES (DATA_PAGES * PAGE + TAIL_BYTES)

static int failures;
static char dir[256];

static void ok(const char *what, int good) {
    printf("  %-46s %s\n", what, good ? "[OK]" : "[FAIL]");
    if (!good) failures++;
}

/* The byte that belongs at `off`. A function of the offset alone, so a page
 * served from the wrong place in the file is wrong in a way this can name. */
static unsigned char byte_at(unsigned long off) {
    unsigned long h = off * 2654435761UL + (off >> 7) * 40503UL;
    return (unsigned char)(h ^ (h >> 11));
}

/* First offset in [from, to) whose mapped byte is wrong, or -1. */
static long first_bad(const unsigned char *p, unsigned long from, unsigned long to) {
    for (unsigned long off = from; off < to; off++) {
        unsigned char want = off < FILE_BYTES ? byte_at(off) : 0;
        if (p[off] != want) return (long)off;
    }
    return -1;
}

/* One byte per page, which is what actually drives the faults. */
static long first_bad_page_probe(const unsigned char *p, unsigned long pages) {
    for (unsigned long i = 0; i < pages; i++) {
        unsigned long off = i * PAGE;
        unsigned char want = off < FILE_BYTES ? byte_at(off) : 0;
        if (p[off] != want) return (long)off;
    }
    return -1;
}

static int write_file(const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1;
    static unsigned char buf[PAGE];
    unsigned long done = 0;
    while (done < FILE_BYTES) {
        unsigned long n = FILE_BYTES - done;
        if (n > PAGE) n = PAGE;
        for (unsigned long i = 0; i < n; i++) buf[i] = byte_at(done + i);
        if (write(fd, buf, n) != (long)n) { close(fd); return -1; }
        done += n;
    }
    close(fd);
    return 0;
}

static void path_in(char *out, size_t n, const char *name) {
    snprintf(out, n, "%s/%s", dir, name);
}

/* 1 + 4: late pages, and a fork child faulting pages nobody has touched. */
static void check_late_and_fork(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { ok("open for the late-page check", 0); return; }
    /* Rounded up to the page that straddles EOF, and **no further**. The tail of
     * that page is specified as zero and is checked below; a page *wholly* past
     * EOF is a different thing — Linux raises `SIGBUS` for it and Akuma serves
     * zeros, a divergence this probe deliberately does not exercise, because
     * doing so kills the probe on the calibration arm (measured: `Bus error`
     * on real Linux, 2026-09-13). */
    unsigned long len = (DATA_PAGES + 1) * PAGE;
    unsigned char *p = mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (p == MAP_FAILED) { ok("mmap for the late-page check", 0); return; }

    /* Page 0 now; everything else after a detour, so the rest are genuinely
     * cold when they are read. */
    ok("first page reads the file", p[0] == byte_at(0));
    for (int i = 0; i < 200; i++) (void)getpid();

    ok("every later page reads the file",
       first_bad_page_probe(p, DATA_PAGES) < 0);
    ok("the page straddling EOF is zero past the end",
       first_bad(p, DATA_PAGES * PAGE, DATA_PAGES * PAGE + PAGE) < 0);
    ok("every byte of the mapping is the file's",
       first_bad(p, 0, FILE_BYTES) < 0);

    /* A child faulting a mapping it inherited but never touched. A fresh
     * mapping, so nothing in it is resident when `fork` copies the record. */
    int fd2 = open(path, O_RDONLY);
    unsigned char *q = fd2 < 0 ? MAP_FAILED
                               : mmap(NULL, DATA_PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd2, 0);
    if (fd2 >= 0) close(fd2);
    if (q == MAP_FAILED) {
        ok("mmap for the fork check", 0);
    } else {
        pid_t kid = fork();
        if (kid == 0) _exit(first_bad_page_probe(q, DATA_PAGES) < 0 ? 0 : 1);
        int st = 0;
        waitpid(kid, &st, 0);
        ok("a fork child faults the file, not zeros", WIFEXITED(st) && WEXITSTATUS(st) == 0);
        /* And the parent's own view is undisturbed by the child's faults. */
        ok("the parent still reads the file after the child", first_bad_page_probe(q, DATA_PAGES) < 0);
        munmap(q, DATA_PAGES * PAGE);
    }
    munmap(p, len);
}

/* 2: the mapping outlives the name, and the inode number is offered to others. */
static void check_unlinked(void) {
    char path[320];
    path_in(path, sizeof path, "mmaplazy.gone");
    if (write_file(path) < 0) { ok("stage the file to unlink", 0); return; }

    int fd = open(path, O_RDONLY);
    if (fd < 0) { ok("open the file to unlink", 0); return; }
    unsigned char *p = mmap(NULL, DATA_PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (p == MAP_FAILED) { ok("mmap the file to unlink", 0); return; }

    /* One page resident, the rest cold — then the name goes away. */
    volatile unsigned char first = p[0];
    (void)first;
    unlink(path);

    /* Churn inode numbers: create, fill and remove several files, so a mapping
     * that does not pin its inode is reading a number somebody else now owns.
     * The bytes written are deliberately NOT this file's pattern. */
    for (int i = 0; i < 8; i++) {
        char other[320];
        char name[64];
        snprintf(name, sizeof name, "mmaplazy.churn%d", i);
        path_in(other, sizeof other, name);
        int o = open(other, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (o >= 0) {
            static unsigned char junk[PAGE];
            memset(junk, 0xA5, sizeof junk);
            for (int k = 0; k < 4; k++) (void)!write(o, junk, sizeof junk);
            close(o);
        }
        unlink(other);
    }

    ok("an unlinked mapping still reads the file",
       first_bad_page_probe(p, DATA_PAGES) < 0);
    munmap(p, DATA_PAGES * PAGE);
}

/* 3: a region split three ways still knows where each piece sits in the file. */
static void check_mprotect_split(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { ok("open for the mprotect split", 0); return; }
    unsigned char *p = mmap(NULL, DATA_PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (p == MAP_FAILED) { ok("mmap for the mprotect split", 0); return; }

    /* Nothing resident yet: the split must survive into the *fill*, not just
     * into the record. Pages 8..12 become unreadable, then readable again. */
    unsigned long mid = 8 * PAGE, mid_len = 4 * PAGE;
    if (mprotect(p + mid, mid_len, PROT_NONE) != 0) {
        ok("mprotect(PROT_NONE) over the middle", 0);
    } else {
        ok("mprotect(PROT_NONE) over the middle", 1);
        /* The head and tail pieces, faulted while the middle is a hole. */
        ok("the head piece reads the file after the split",
           first_bad_page_probe(p, 8) < 0);
        int bad = 0;
        for (unsigned long i = 12; i < DATA_PAGES; i++) {
            unsigned long off = i * PAGE;
            if (p[off] != byte_at(off)) { bad = 1; break; }
        }
        ok("the tail piece reads the file after the split", !bad);
        ok("mprotect back to PROT_READ", mprotect(p + mid, mid_len, PROT_READ) == 0);
        ok("the restored middle reads its own part of the file",
           first_bad(p, mid, mid + mid_len) < 0);
    }
    munmap(p, DATA_PAGES * PAGE);
}

/* Sharing, from the outside: two mappings here, and one more in a child. */
static void check_shared_views(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { ok("open for the shared-view check", 0); return; }
    unsigned char *a = mmap(NULL, DATA_PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    unsigned char *b = mmap(NULL, DATA_PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (a == MAP_FAILED || b == MAP_FAILED) { ok("two mappings of one file", 0); return; }
    ok("two mappings of one file", 1);
    ok("the first view reads the file", first_bad_page_probe(a, DATA_PAGES) < 0);
    ok("the second view reads the file", first_bad_page_probe(b, DATA_PAGES) < 0);

    pid_t kid = fork();
    if (kid == 0) {
        int cfd = open(path, O_RDONLY);
        if (cfd < 0) _exit(2);
        unsigned char *c = mmap(NULL, DATA_PAGES * PAGE, PROT_READ, MAP_PRIVATE, cfd, 0);
        close(cfd);
        if (c == MAP_FAILED) _exit(3);
        _exit(first_bad_page_probe(c, DATA_PAGES) < 0 ? 0 : 1);
    }
    int st = 0;
    waitpid(kid, &st, 0);
    ok("a second process mapping the same file reads it",
       WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* And the first process's views are still intact once the sharer is gone —
     * a mishandled reference would have freed the frames under them. */
    ok("both views survive the other process exiting",
       first_bad_page_probe(a, DATA_PAGES) < 0 && first_bad_page_probe(b, DATA_PAGES) < 0);
    munmap(a, DATA_PAGES * PAGE);
    munmap(b, DATA_PAGES * PAGE);
}

int main(int argc, char **argv) {
    snprintf(dir, sizeof dir, "%s", argc > 1 ? argv[1] : "/tmp");
    printf("mmaplazy: file %lu bytes (%lu pages + %lu), dir %s\n",
           FILE_BYTES, DATA_PAGES, TAIL_BYTES, dir);

    char path[320];
    path_in(path, sizeof path, "mmaplazy.dat");
    if (write_file(path) < 0) {
        printf("mmaplazy: cannot stage %s\n", path);
        return 2;
    }

    check_late_and_fork(path);
    check_unlinked();
    check_mprotect_split(path);
    check_shared_views(path);

    unlink(path);
    printf("mmaplazy: %s (%d failure%s)\n", failures ? "FAIL" : "PASS",
           failures, failures == 1 ? "" : "s");
    return failures ? 1 : 0;
}
