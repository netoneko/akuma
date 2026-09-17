/* execleak2 — bisects the smpstress memory drain by cycle shape.
 *
 * Stage 1 (execleak.c) showed plain fork+exec+waitpid of a small ELF leaks
 * nothing. The smpstress FC runs drained 2.5 GB to the OOM floor within one
 * PSTATS sweep, so the leak needs more of the context. This program walks
 * the differences one at a time, mode per run (INITARGS=<mode>):
 *
 *   mt       — parent spawns 2 threads holding 64 MiB mappings each; the
 *              MAIN thread forks children that just _exit. Isolates
 *              fork+teardown of a big multithreaded address space.
 *   mtexec   — same, but each child execv's /bin/hello before exiting.
 *              Adds replace-image + new-image teardown on top.
 *   madv     — single-threaded; per cycle: madvise(MADV_DONTNEED) half a
 *              4 MiB region, refill, mmap/munmap a scratch region. Isolates
 *              the anon churn without fork.
 *
 * Each prints freeram (sysinfo) every PRINT_EVERY cycles.
 *
 * Build: x86_64-linux-musl-gcc -static -O2 -pthread -o execleak2 execleak2.c
 * Run:   INIT=/probes/execleak2 INITARGS=mt|mtexec|madv
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <sys/sysinfo.h>
#include <errno.h>
#include <fcntl.h>

#define CYCLES      200
#define PRINT_EVERY 25
#define THREAD_MAP  (64u * 1024 * 1024)

static void report(int n);

static unsigned char *regions_churn[2];
static void fill_pattern(unsigned char *p, size_t n, uint64_t seed) {
    for (size_t i = 0; i < n; i += 8) {
        uint64_t v = seed + i;
        v ^= v >> 33; v *= 0xff51afd7ed558ccdULL;
        v ^= v >> 33; v *= 0xc4ceb9fe1a85ec53ULL;
        v ^= v >> 33;
        size_t k = (n - i < 8) ? n - i : 8;
        __builtin_memcpy(p + i, &v, k);
    }
}

static void *file_checker_worker(void *arg) {
    (void)arg;
    for (;;) {
        int fd = open("/tmp/execleak2.bin", O_RDONLY);
        if (fd < 0) return NULL;
        void *m = mmap(NULL, 256 * 1024, PROT_READ, MAP_PRIVATE, fd, 0);
        if (m != MAP_FAILED) {
            volatile unsigned char sum = 0;
            for (size_t i = 0; i < 256 * 1024; i += 4096) sum += ((unsigned char *)m)[i];
            munmap(m, 256 * 1024);
        }
        close(fd);
        usleep(50000);
    }
}

static void *exec_churner_worker(void *arg) {
    (void)arg;
    for (;;) {
        pid_t pid = fork();
        if (pid == 0) {
            char *argv2[] = {"hello", NULL};
            execv("/bin/hello", argv2);
            _exit(127);
        }
        if (pid > 0) { int st = 0; waitpid(pid, &st, 0); }
        usleep(20000);
    }
}

static unsigned char *maps[2];
static volatile int threads_ready;
static volatile int stop_forkers;

/* fork from a NON-MAIN thread — the shape smpstress's exec churner uses and
 * every mode above avoided. Children _exit (mode mtfork) or exec /bin/hello
 * (mode mtforkexec). */
static void *forker(void *arg) {
    int do_exec = (int)(long)arg;
    for (int n = 1; n <= CYCLES && !stop_forkers; n++) {
        pid_t pid = fork();
        if (pid < 0) { printf("execleak2: forker fork failed at %d errno=%d\n", n, errno); fflush(stdout); return NULL; }
        if (pid == 0) {
            if (do_exec) {
                char *argv2[] = {"hello", NULL};
                execv("/bin/hello", argv2);
                _exit(127);
            }
            _exit(0);
        }
        int st = 0;
        waitpid(pid, &st, 0);
        if (st != ((do_exec) ? 0x7f << 8 : 0)) {
            printf("execleak2: forker cycle %d bad status 0x%x\n", n, st);
            fflush(stdout);
            return NULL;
        }
        if (n % PRINT_EVERY == 0) report(n);
    }
    return NULL;
}

static int replica_g; /* 'g' = grandchild fork from the churn thread */

static void *holder(void *arg) {
    long i = (long)arg;
    for (size_t off = 0; off < THREAD_MAP; off += 4096) maps[i][off] = (unsigned char)off;
    __atomic_add_fetch(&threads_ready, 1, __ATOMIC_SEQ_CST);
    unsigned char *r = regions_churn[i];
    if (!r) { for (;;) usleep(1000000); }
    /* The smpstress churn thread: refill/madvise the region while the
     * SIBLING thread does the same, and fork+CoW-write grandchildren from
     * under it — the fork share pass races a sibling's fault-in/madvise. */
    for (unsigned long it = 0;; it++) {
        fill_pattern(r, 512 * 1024, 0x1234 + (uint64_t)i);
        if (madvise(r + 256 * 1024, 256 * 1024, MADV_DONTNEED) == 0)
            fill_pattern(r + 256 * 1024, 256 * 1024, 0x1234 + (uint64_t)i);
        if (replica_g && (it % 16) == 8) {
            pid_t pid = fork();
            if (pid == 0) {
                for (size_t off = 0; off < 512 * 1024; off += 4096) r[off] ^= 0x5a;
                _exit(0);
            }
            if (pid > 0) { int st = 0; waitpid(pid, &st, 0); }
        }
        if ((it % 50) == 0) usleep(2000);
    }
    return NULL;
}

static void report(int n) {
    struct sysinfo si;
    sysinfo(&si);
    printf("execleak2: %s %d cycles freeram=%lu MB\n", getenv("MODE") ?: "?",
           n, si.freeram >> 20);
    fflush(stdout);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "mt";
    static char modename[16];
    snprintf(modename, sizeof modename, "%s", mode);
    setenv("MODE", modename, 1);

    if (strncmp(mode, "w4", 2) == 0) {
        /* Four concurrent replicas — smpstress's actual shape. Same knobs in
         * the rest of the mode string: w4gx / w4g / ... */
        const char *sub = mode + 2; /* "wgx" etc */
        static char subname[16];
        snprintf(subname, sizeof subname, "%s", sub);
        pid_t kids[4];
        for (int i = 0; i < 4; i++) {
            kids[i] = fork();
            if (kids[i] == 0) {
                char *argv2[] = {"execleak2", subname, NULL};
                execv("/probes/execleak2", argv2);
                _exit(127);
            }
        }
        for (int i = 0; i < 4; i++) { int st = 0; waitpid(kids[i], &st, 0); }
        printf("execleak2: DONE w4\n");
        fflush(stdout);
        return 0;
    }

    if (strncmp(mode, "w", 1) == 0) {
        /* One smpstress worker, faithfully: 2 threads each holding a 64 MiB
         * ballast + a 512 KiB churn region (madvise DONTNEED + refill), a
         * shared file mapping, plus the removable parts the INITARGS string
         * names: 'f' = file-checker thread, 'x' = exec churner thread,
         * 'g' = grandchild fork that CoW-writes the churn region. */
        const int want_f = strchr(mode, 'f') != NULL;
        const int want_x = strchr(mode, 'x') != NULL;
        const int want_g = strchr(mode, 'g') != NULL;
        replica_g = want_g;
        struct warg { int t; unsigned char *ballast; unsigned char *region; };
        static struct warg wa[2];
        for (int i = 0; i < 2; i++) {
            wa[i].t = i;
            wa[i].ballast = mmap(NULL, THREAD_MAP, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            wa[i].region = mmap(NULL, 512 * 1024, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (wa[i].ballast == MAP_FAILED || wa[i].region == MAP_FAILED) { perror("mmap"); return 1; }
            maps[i] = wa[i].ballast; /* holder() fills the global maps[] */
            regions_churn[i] = wa[i].region;
        }
        pthread_t hold[2];
        for (long i = 0; i < 2; i++) pthread_create(&hold[i], NULL, holder, (void *)i);
        pthread_t extra[2];
        int nextra = 0;
        if (want_f) pthread_create(&extra[nextra++], NULL, file_checker_worker, NULL);
        if (want_x) pthread_create(&extra[nextra++], NULL, exec_churner_worker, NULL);
        report(0);
        for (int n = 1; n <= 60; n++) {
            usleep(100000);
            report(n * 20);
        }
        (void)hold; (void)extra; (void)want_g;
        report(1200);
        printf("execleak2: DONE mode=%s\n", mode);
        fflush(stdout);
        return 0;
    }

    if (strcmp(mode, "remap") == 0) {
        /* Isolates mapper-reference accounting: write the file ONCE (so the
         * cache populates), then open+mmap+read+munmap in a loop. No writes,
         * so invalidate never runs; every cycle is a pure cache-hit mapping.
         * Flat freeram means the mapper's reference is balanced across
         * munmap; a slope of one file per cycle means munmap never drops
         * the mapper's CoW reference on a shared file frame. */
        const char *path = "/tmp/execleak2.bin";
        static unsigned char buf[256 * 1024];
        FILE *f0 = fopen(path, "w");
        fwrite(buf, 1, sizeof buf, f0);
        fclose(f0);
        /* fault it in once so the cache holds it */
        {
            int fd = open(path, O_RDONLY);
            void *m = mmap(NULL, sizeof buf, PROT_READ, MAP_PRIVATE, fd, 0);
            volatile unsigned char sum = 0;
            for (size_t i = 0; i < sizeof buf; i += 4096) sum += ((unsigned char *)m)[i];
            munmap(m, sizeof buf);
            close(fd);
        }
        report(0);
        for (int n = 1; n <= CYCLES * 40; n++) {
            int fd = open(path, O_RDONLY);
            void *m = mmap(NULL, sizeof buf, PROT_READ, MAP_PRIVATE, fd, 0);
            if (m == MAP_FAILED) { printf("execleak2: mmap failed\n"); return 1; }
            volatile unsigned char sum = 0;
            for (size_t i = 0; i < sizeof buf; i += 4096) sum += ((unsigned char *)m)[i];
            munmap(m, sizeof buf);
            close(fd);
            if (n % 100 == 0) usleep(2000);
            if (n % PRINT_EVERY == 0) report(n);
        }
        report(CYCLES * 40);
        printf("execleak2: DONE mode=remap\n");
        fflush(stdout);
        return 0;
    }

    if (strcmp(mode, "file") == 0) {
        /* Isolates the file-rewrite churn: one writer thread rewrites the
         * file (new content each version — a fresh fpcache fill each time if
         * the cache keys on (inode,offset) but never evicts), while the main
         * thread re-maps and reads it. No fork at all. */
        const char *path = "/tmp/execleak2.bin";
        static unsigned char buf[256 * 1024];
        FILE *f0 = fopen(path, "w");
        fwrite(buf, 1, sizeof buf, f0);
        fclose(f0);
        report(0);
        for (int n = 1; n <= CYCLES * 80; n++) {
            for (size_t i = 0; i < sizeof buf; i += 4096) buf[i] = (unsigned char)n;
            FILE *f = fopen(path, "r+");
            if (!f) { printf("execleak2: fopen failed\n"); return 1; }
            fwrite(buf, 1, sizeof buf, f);
            fclose(f);
            int fd = open(path, O_RDONLY);
            void *m = mmap(NULL, sizeof buf, PROT_READ, MAP_PRIVATE, fd, 0);
            if (m != MAP_FAILED) {
                volatile unsigned char sum = 0;
                for (size_t i = 0; i < sizeof buf; i += 4096) sum += ((unsigned char *)m)[i];
                munmap(m, sizeof buf);
            }
            close(fd);
            if (n % 100 == 0) usleep(2000); /* let the idle loop run its sweep */
            if (n % PRINT_EVERY == 0) report(n);
        }
        report(CYCLES * 80);
        printf("execleak2: DONE mode=file\n");
        fflush(stdout);
        return 0;
    }

    if (strncmp(mode, "mtfork", 6) == 0) {
        for (int i = 0; i < 2; i++) {
            maps[i] = mmap(NULL, THREAD_MAP, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (maps[i] == MAP_FAILED) { perror("mmap"); return 1; }
        }
        pthread_t hold[2];
        for (long i = 0; i < 2; i++) pthread_create(&hold[i], NULL, holder, (void *)i);
        while (__atomic_load_n(&threads_ready, __ATOMIC_SEQ_CST) < 2) usleep(1000);
        pthread_t fk;
        pthread_create(&fk, NULL, forker, (void *)(long)(strcmp(mode, "mtforkexec") == 0));
        pthread_join(fk, NULL);
        stop_forkers = 1;
        report(CYCLES);
        printf("execleak2: DONE mode=%s\n", mode);
        fflush(stdout);
        return 0;
    }

    if (strcmp(mode, "madv") != 0) {
        for (int i = 0; i < 2; i++) {
            maps[i] = mmap(NULL, THREAD_MAP, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (maps[i] == MAP_FAILED) { perror("mmap"); return 1; }
        }
        pthread_t th[2];
        for (long i = 0; i < 2; i++) pthread_create(&th[i], NULL, holder, (void *)i);
        while (__atomic_load_n(&threads_ready, __ATOMIC_SEQ_CST) < 2) usleep(1000);
    }

    unsigned char *scratch = mmap(NULL, 4u * 1024 * 1024, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    struct sysinfo si;
    sysinfo(&si);
    printf("execleak2: start mode=%s freeram=%lu MB\n", mode, si.freeram >> 20);
    fflush(stdout);

    for (int n = 1; n <= CYCLES; n++) {
        if (strcmp(mode, "madv") == 0) {
            if (madvise(scratch + 2u * 1024 * 1024, 2u * 1024 * 1024, MADV_DONTNEED) != 0) {
                printf("execleak2: madvise failed %d\n", errno); return 1;
            }
            memset(scratch + 2u * 1024 * 1024, 0x5a, 2u * 1024 * 1024);
            memset(scratch, 0xa5, 2u * 1024 * 1024);
            munmap(scratch, 4u * 1024 * 1024);
            scratch = mmap(NULL, 4u * 1024 * 1024, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (scratch == MAP_FAILED) { perror("mmap scratch"); return 1; }
        } else {
            pid_t pid = fork();
            if (pid < 0) {
                printf("execleak2: fork failed at %d errno=%d\n", n, errno);
                fflush(stdout);
                return 1;
            }
            if (pid == 0) {
                if (strcmp(mode, "mtexec") == 0) {
                    char *argv2[] = {"hello", NULL};
                    execv("/bin/hello", argv2);
                    _exit(127);
                }
                _exit(0);
            }
            int st = 0;
            waitpid(pid, &st, 0);
            if (st != ((strcmp(mode, "mtexec") == 0) ? 0x7f << 8 : 0)) {
                printf("execleak2: cycle %d bad status 0x%x\n", n, st);
                fflush(stdout);
                return 1;
            }
        }
        if (n % PRINT_EVERY == 0) report(n);
    }
    report(CYCLES);
    return 0;
}
