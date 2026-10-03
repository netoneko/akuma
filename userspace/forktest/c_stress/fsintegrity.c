// fsintegrity: does a writable-MAP_SHARED workload corrupt unrelated ext2 directories?
//
// Field report (2026-10-03, the trashcan): `git commit` in a repo failed with
// "unable to write file .git/objects/7d/...: No such file or directory", and the
// next `git status` said ".git" was not there at all. Suspects: the shared
// writable page table (akuma-fpcache-rw), ext2 ftruncate-extend, the flush paths.
//
// Two kinds of worker run at once on ONE filesystem under ROOT:
//   gitsim  - mkdir objects/XX, create a tmp file, write it, rename it into
//             place, replace an index by lock-file + rename. Keeps a manifest;
//             re-verifies every directory and file it ever made, continuously.
//   mapsim  - what rustc/memmap2/SQLite do: create, ftruncate-EXTEND, mmap
//             MAP_SHARED|PROT_WRITE, store, then munmap / msync / unlink-before-
//             unmap / rename-over / _exit with the mapping live. Several
//             processes map the same file, one of them forks from a thread.
//
// The gitsim workers are the canary: any directory or object they made that
// stops resolving, or reads back different bytes, is reported with the step.
// Afterwards the host runs e2fsck on the image; that is the other half.
//
//   fsintegrity <root> <seconds> <gitsim-workers> <mapsim-workers>
//   mode flags via env FSI_NOMAP=1 (skip mapsim) to get a control arm.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *ROOT;
static int SECS;

static uint64_t rng_state;
static uint32_t rnd(void) {
    rng_state ^= rng_state << 13; rng_state ^= rng_state >> 7; rng_state ^= rng_state << 17;
    return (uint32_t)(rng_state >> 11);
}
static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* ---------------------------------------------------------------- gitsim */

#define MAXOBJ 4096
struct obj { char dir[8]; char name[48]; uint32_t seed; uint32_t len; };

static void fill(unsigned char *b, uint32_t seed, uint32_t len) {
    uint32_t x = seed * 2654435761u + 1;
    for (uint32_t i = 0; i < len; i++) { x = x * 1664525u + 1013904223u; b[i] = x >> 24; }
}

static int bad;
#define FAIL(...) do { bad++; printf("FAIL[%d] ", getpid()); printf(__VA_ARGS__); printf(" (errno=%d %s)\n", errno, strerror(errno)); fflush(stdout); } while (0)

static int verify_obj(const char *repo, const struct obj *o) {
    char p[256]; unsigned char buf[4096], want[4096];
    snprintf(p, sizeof p, "%s/.git/objects/%s/%s", repo, o->dir, o->name);
    int fd = open(p, O_RDONLY);
    if (fd < 0) { FAIL("object missing: %s", p); return -1; }
    ssize_t n = read(fd, buf, sizeof buf);
    close(fd);
    if (n != (ssize_t)o->len) { FAIL("object %s short/long: %zd != %u", p, n, o->len); return -1; }
    fill(want, o->seed, o->len);
    if (memcmp(buf, want, o->len)) { FAIL("object %s content differs", p); return -1; }
    return 0;
}

static int verify_all(const char *repo, struct obj *objs, int n) {
    struct stat st; char p[256];
    snprintf(p, sizeof p, "%s/.git", repo);
    if (stat(p, &st) || !S_ISDIR(st.st_mode)) { FAIL(".git gone: %s", p); return -1; }
    snprintf(p, sizeof p, "%s/.git/index", repo);
    if (stat(p, &st)) { FAIL("index gone: %s", p); return -1; }
    for (int i = 0; i < n; i++) if (verify_obj(repo, &objs[i])) return -1;
    return 0;
}

static int gitsim(int id) {
    char repo[200];
    snprintf(repo, sizeof repo, "%s/repo%d", ROOT, id);
    char p[300];
    snprintf(p, sizeof p, "%s", repo); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/.git", repo); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/.git/objects", repo); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/src", repo); mkdir(p, 0755);

    static struct obj objs[MAXOBJ];
    int n = 0, commits = 0;
    double end = now() + SECS;
    rng_state = 0x9e3779b97f4a7c15ull ^ (uint64_t)id * 7919;
    while (now() < end && n < MAXOBJ - 4) {
        /* one "commit": 3 loose objects, an index replace, a worktree file rewrite */
        for (int k = 0; k < 3; k++) {
            struct obj *o = &objs[n];
            uint32_t h = rnd();
            snprintf(o->dir, sizeof o->dir, "%02x", h & 0xff);
            snprintf(o->name, sizeof o->name, "%08x%08x", h, (unsigned)n);
            o->seed = h ^ (uint32_t)n; o->len = 60 + rnd() % 3000;
            snprintf(p, sizeof p, "%s/.git/objects/%s", repo, o->dir);
            mkdir(p, 0755); /* EEXIST is fine, that is what git does */
            char tmp[300], fin[300];
            snprintf(tmp, sizeof tmp, "%s/.git/objects/tmp_obj_%d_%d", repo, id, n);
            snprintf(fin, sizeof fin, "%s/.git/objects/%s/%s", repo, o->dir, o->name);
            int fd = open(tmp, O_WRONLY | O_CREAT | O_EXCL, 0444);
            if (fd < 0) { FAIL("create tmp %s", tmp); return 1; }
            unsigned char buf[4096]; fill(buf, o->seed, o->len);
            if (write(fd, buf, o->len) != (ssize_t)o->len) { FAIL("write tmp"); return 1; }
            if (rnd() % 4 == 0) fsync(fd);
            close(fd);
            if (rename(tmp, fin)) { FAIL("rename %s -> %s", tmp, fin); return 1; }
            n++;
        }
        /* index.lock then rename over index */
        char lock[300], idx[300];
        snprintf(lock, sizeof lock, "%s/.git/index.lock", repo);
        snprintf(idx, sizeof idx, "%s/.git/index", repo);
        int fd = open(lock, O_WRONLY | O_CREAT | O_EXCL, 0644);
        if (fd < 0) { FAIL("index.lock"); return 1; }
        unsigned char ib[1024]; fill(ib, commits, sizeof ib);
        if (write(fd, ib, sizeof ib) != sizeof ib) { FAIL("index write"); return 1; }
        close(fd);
        if (rename(lock, idx)) { FAIL("rename index"); return 1; }
        /* the worktree file, rewritten in place and truncated, as an editor does */
        snprintf(p, sizeof p, "%s/src/main.rs", repo);
        fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd >= 0) { if (write(fd, ib, 512) < 0) {} close(fd); }
        commits++;
        /* the canary: check the whole tree every few commits, and every dir right away */
        snprintf(p, sizeof p, "%s/.git", repo);
        struct stat st;
        if (stat(p, &st)) { FAIL(".git vanished after commit %d", commits); return 1; }
        if (commits % 8 == 0 && verify_all(repo, objs, n)) { printf("gitsim %d: corrupt after %d commits\n", id, commits); return 1; }
    }
    int r = verify_all(repo, objs, n);
    printf("gitsim %d: %d commits, %d objects, %s\n", id, commits, n, r ? "CORRUPT" : "ok");
    return r ? 1 : 0;
}

/* ---------------------------------------------------------------- mapsim */

struct tparm { const char *path; size_t len; };
static void *thread_fork(void *a) {
    struct tparm *t = a;
    pid_t c = fork();
    if (c == 0) {
        int fd = open(t->path, O_RDWR);
        if (fd >= 0) {
            volatile unsigned char *m = mmap(0, t->len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            if (m != MAP_FAILED) { m[0]++; m[t->len - 1]++; }
        }
        _exit(0); /* mapping live */
    }
    if (c > 0) waitpid(c, 0, 0);
    return 0;
}

static int mapsim(int id, int shared_file) {
    char path[300], other[300];
    rng_state = 0xdeadbeefcafef00dull ^ (uint64_t)id * 104729;
    double end = now() + SECS;
    int iter = 0;
    while (now() < end) {
        size_t pages = 1 + rnd() % 24, len = pages * 4096;
        if (shared_file) snprintf(path, sizeof path, "%s/shared-shm", ROOT);
        else snprintf(path, sizeof path, "%s/map%d-%d.rmeta", ROOT, id, iter);
        snprintf(other, sizeof other, "%s.tmp", path);
        int fd = open(path, O_RDWR | O_CREAT, 0644);
        if (fd < 0) { FAIL("mapsim open %s", path); return 1; }
        struct stat st; fstat(fd, &st);
        if ((size_t)st.st_size < len && ftruncate(fd, len)) { FAIL("ftruncate-extend %s to %zu", path, len); return 1; }
        unsigned char *m = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (m == MAP_FAILED) { FAIL("mmap %s", path); return 1; }
        for (size_t p = 0; p < len; p += 4096) memset(m + p, (int)(rnd() & 0xff), 256 + rnd() % 3000);
        if (rnd() % 3 == 0) {
            /* extend while mapped, as SQLite grows -shm, by one-byte pwrite at the new end */
            size_t nl = len + 4096 * (1 + rnd() % 3);
            if (pwrite(fd, "\0", 1, nl - 1) != 1) FAIL("pwrite-extend");
        }
        if (rnd() % 5 == 0) { pthread_t t; struct tparm tp = { path, len };
            pthread_create(&t, 0, thread_fork, &tp); pthread_join(t, 0); }
        switch (rnd() % 6) {
        case 0: msync(m, len, MS_SYNC); munmap(m, len); close(fd); break;
        case 1: munmap(m, len); close(fd); break;
        case 2: if (!shared_file) unlink(path); munmap(m, len); close(fd); break;       /* SQLite -shm */
        case 3: close(fd); if (!shared_file) { rename(path, other); unlink(other); } munmap(m, len); break;
        case 4: ftruncate(fd, 0); munmap(m, len); close(fd); break;                       /* shrink under mapping */
        case 5: if (fork() == 0) _exit(0); wait(0); close(fd); /* leak the mapping to process exit */
                if (iter % 4 == 3) _exit(0); break;
        }
        if (!shared_file && rnd() % 2) unlink(path);
        iter++;
        if (iter % 64 == 0) { /* bounded footprint */
            DIR *d = opendir(ROOT); struct dirent *e; char q[400];
            while (d && (e = readdir(d))) if (strstr(e->d_name, ".rmeta")) { snprintf(q, sizeof q, "%s/%s", ROOT, e->d_name); unlink(q); }
            if (d) closedir(d);
        }
    }
    printf("mapsim %d: %d iterations\n", id, iter);
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 5) { fprintf(stderr, "usage: %s <root> <seconds> <gitsim> <mapsim>\n", argv[0]); return 2; }
    ROOT = argv[1]; SECS = atoi(argv[2]);
    int ng = atoi(argv[3]), nm = atoi(argv[4]);
    setvbuf(stdout, 0, _IOLBF, 0);
    mkdir(ROOT, 0755);
    int kids = 0;
    if (getenv("FSI_HOG_MB")) { /* memory pressure: hold this many MiB of touched anonymous pages */
        if (fork() == 0) {
            size_t want = (size_t)atoi(getenv("FSI_HOG_MB")) << 20, got = 0;
            while (got < want) {
                unsigned char *m = mmap(0, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (m == MAP_FAILED) break;
                for (int o = 0; o < (1 << 20); o += 4096) m[o] = 1;
                got += 1 << 20;
            }
            printf("hog: holding %zu MiB\n", got >> 20);
            sleep(SECS + 5);
            _exit(0);
        }
        kids++;
    }
    for (int i = 0; i < ng; i++) if (fork() == 0) _exit(gitsim(i));  else kids++;
    if (!getenv("FSI_NOMAP"))
        for (int i = 0; i < nm; i++) if (fork() == 0) _exit(mapsim(i, i % 2 == 1)); else kids++;
    int fail = 0, st;
    while (kids-- > 0) {
        pid_t w = wait(&st);
        if (!WIFEXITED(st) || WEXITSTATUS(st)) {
            fail++;
            if (WIFSIGNALED(st)) printf("worker %d killed by signal %d\n", w, WTERMSIG(st));
            else printf("worker %d exited %d\n", w, WEXITSTATUS(st));
        }
    }
    printf("fsintegrity: %s (%d failing workers)\n", fail ? "FAIL" : "PASS", fail);
    return fail ? 1 : 0;
}
