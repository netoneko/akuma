/*
 * killtree: SIGKILL/SIGTERM a Chromium-shaped process tree while its threads
 * are parked in every blocking syscall family Chromium uses, from another
 * process on another core, many rounds — and check that the kernel survives,
 * reaps the tree promptly, and does not leak the kill onto bystanders.
 *
 * The tree: a "browser" (its own process group) with WORKERS threads, which
 * forks a "zygote" with the same threads, which forks a "renderer" with the
 * same threads. Each member's threads:
 *   futex    untimed FUTEX_WAIT on a word that never changes
 *   epoll    epoll_wait(-1) on a pipe that never gets data
 *   ppoll    ppoll(NULL timeout) on another silent pipe
 *   pipe     read() on a silent pipe
 *   unix     recvmsg() on an AF_UNIX SOCK_SEQPACKET socketpair, after one
 *            SCM_RIGHTS exchange (the fd-passing shape)
 *   spin     a pure compute loop, no syscall (only a tick can reach it)
 *   sleep    nanosleep(1 ms) in a loop
 *   shm      a MAP_SHARED mapping of an unlinked /tmp file, written in a loop
 * (Chromium: ~90-120 threads over ~8 processes; this is 3 x (1 + 8) = 27.)
 *
 * The bystander: an unrelated process (its own group) whose threads park in
 * the same families and count every wait that returns an error. A kill aimed
 * at the tree must never show up there — that is the recycled-slot / stale-row
 * class (`docs/archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md`, `STALE_THREAD_SLOT_KILL.md`).
 *
 * Modes (argv[2]; 0 = round-robin over all of them):
 *   1  kill(browser, SIGKILL)                 — `kill -9 <browser pid>`
 *   2  kill(-pgid, SIGKILL)                   — kami's group kill
 *   3  kill(-pgid, SIGTERM), 300 ms, SIGKILL  — kami's gentle order
 *   4  kill(<a worker thread's tid>, SIGKILL) — `kill -9` of a thread pid
 *
 * Verdict per round: time from the kill to the browser being reaped, and
 * whether every tree member is gone (zombie or absent) within LIMIT_MS. The
 * probe prints PASS only if every round reaped and the bystander counted 0
 * spurious errors. Linux is the expectation; run it there first.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define WORKERS 8
#define LIMIT_MS 10000
#define SETTLE_MS 300
#define SHM_BYTES (64 * 1024)

static long ms_since(const struct timespec *t0)
{
    struct timespec t1;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    return (t1.tv_sec - t0->tv_sec) * 1000L + (t1.tv_nsec - t0->tv_nsec) / 1000000L;
}

static void msleep(long ms)
{
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

/* ------------------------------------------------------------------------ */
/* The worker threads. `spurious` is non-NULL only in the bystander, which
 * counts waits that return an error; tree members never return from theirs
 * (the kill ends them) so nothing is counted there. */

struct ctx {
    int futex_word;
    int silent[4][2];      /* pipes for epoll, ppoll, read, and the unix peer's sync */
    int unix_sock[2];
    volatile long *spurious;
    volatile int quit;
    int tids[WORKERS];
    volatile int ready;
};

static void note(struct ctx *c, const char *who, int err)
{
    if (c->spurious) {
        __sync_fetch_and_add(c->spurious, 1);
        fprintf(stderr, "bystander: %s returned errno %d (%s)\n", who, err, strerror(err));
    }
}

static void *w_futex(void *a)
{
    struct ctx *c = a;
    for (;;) {
        long r = syscall(SYS_futex, &c->futex_word, FUTEX_WAIT, 0, NULL, NULL, 0);
        if (c->quit) break;
        if (r < 0 && errno != EAGAIN) note(c, "futex", errno);
    }
    return NULL;
}

static void *w_epoll(void *a)
{
    struct ctx *c = a;
    int ep = epoll_create1(0);
    struct epoll_event ev = { .events = EPOLLIN, .data.fd = c->silent[0][0] };
    epoll_ctl(ep, EPOLL_CTL_ADD, c->silent[0][0], &ev);
    for (;;) {
        struct epoll_event out;
        int r = epoll_wait(ep, &out, 1, -1);
        if (c->quit) break;
        if (r < 0) note(c, "epoll_wait", errno);
    }
    return NULL;
}

static void *w_ppoll(void *a)
{
    struct ctx *c = a;
    struct pollfd p = { .fd = c->silent[1][0], .events = POLLIN };
    for (;;) {
        int r = ppoll(&p, 1, NULL, NULL);
        if (c->quit) break;
        if (r < 0) note(c, "ppoll", errno);
    }
    return NULL;
}

static void *w_pipe(void *a)
{
    struct ctx *c = a;
    char b;
    for (;;) {
        ssize_t r = read(c->silent[2][0], &b, 1);
        if (c->quit) break;
        if (r < 0) note(c, "read", errno);
    }
    return NULL;
}

static void *w_unix(void *a)
{
    struct ctx *c = a;
    /* One SCM_RIGHTS exchange first: pass the silent pipe's read end to the
     * peer side of the socketpair and read it back, so the fd-passing path
     * has run in this process before the kill lands. */
    {
        char buf[1] = { 'x' };
        char cbuf[CMSG_SPACE(sizeof(int))];
        struct iovec iov = { buf, 1 };
        struct msghdr m = { 0 };
        m.msg_iov = &iov; m.msg_iovlen = 1;
        m.msg_control = cbuf; m.msg_controllen = sizeof cbuf;
        struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
        cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS;
        cm->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cm), &c->silent[3][0], sizeof(int));
        if (sendmsg(c->unix_sock[1], &m, 0) == 1) {
            m.msg_controllen = sizeof cbuf;
            if (recvmsg(c->unix_sock[0], &m, 0) == 1) {
                cm = CMSG_FIRSTHDR(&m);
                if (cm && cm->cmsg_type == SCM_RIGHTS) {
                    int got; memcpy(&got, CMSG_DATA(cm), sizeof got);
                    close(got);
                }
            }
        }
    }
    for (;;) {
        char buf[16];
        struct iovec iov = { buf, sizeof buf };
        struct msghdr m = { 0 };
        m.msg_iov = &iov; m.msg_iovlen = 1;
        ssize_t r = recvmsg(c->unix_sock[0], &m, 0);
        if (c->quit) break;
        if (r < 0) note(c, "recvmsg", errno);
        if (r == 0) break;
    }
    return NULL;
}

static void *w_spin(void *a)
{
    struct ctx *c = a;
    volatile unsigned long x = 1;
    while (!c->quit) x = x * 2862933555777941757UL + 3037000493UL;
    return NULL;
}

static void *w_sleep(void *a)
{
    struct ctx *c = a;
    while (!c->quit) {
        struct timespec ts = { 0, 1000000L };
        if (nanosleep(&ts, NULL) < 0 && errno != EINTR) note(c, "nanosleep", errno);
    }
    return NULL;
}

static void *w_shm(void *a)
{
    struct ctx *c = a;
    char path[64];
    snprintf(path, sizeof path, "/tmp/killtree.%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) { note(c, "open", errno); return NULL; }
    unlink(path);
    if (ftruncate(fd, SHM_BYTES) < 0) { note(c, "ftruncate", errno); close(fd); return NULL; }
    unsigned char *m = mmap(NULL, SHM_BYTES, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) { note(c, "mmap", errno); close(fd); return NULL; }
    unsigned long i = 0;
    while (!c->quit) {
        m[(i * 4099) % SHM_BYTES] = (unsigned char)i;
        if ((++i & 0xffff) == 0) msync(m, SHM_BYTES, MS_ASYNC);
    }
    munmap(m, SHM_BYTES);
    close(fd);
    return NULL;
}

static void *(*const WORK[WORKERS])(void *) = {
    w_futex, w_epoll, w_ppoll, w_pipe, w_unix, w_spin, w_sleep, w_shm,
};
static const char *const WORK_NAME[WORKERS] = {
    "futex", "epoll", "ppoll", "pipe", "unix", "spin", "sleep", "shm",
};

struct tramp { struct ctx *c; int i; };

static void *tramp_fn(void *a)
{
    struct tramp *t = a;
    struct ctx *c = t->c;
    int i = t->i;
    free(t);
    c->tids[i] = (int)syscall(SYS_gettid);
    __sync_fetch_and_add(&c->ready, 1);
    return WORK[i](c);
}

static int start_workers(struct ctx *c, pthread_t *th)
{
    for (int i = 0; i < 4; i++)
        if (pipe(c->silent[i]) < 0) return -1;
    if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, c->unix_sock) < 0) {
        /* SEQPACKET missing is a different gap; fall back so the rest runs. */
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, c->unix_sock) < 0) return -1;
    }
    for (int i = 0; i < WORKERS; i++) {
        struct tramp *t = malloc(sizeof *t);
        t->c = c; t->i = i;
        if (pthread_create(&th[i], NULL, tramp_fn, t) != 0) return -1;
    }
    return 0;
}

/* ------------------------------------------------------------------------ */
/* A tree member: start the workers, report (pid, worker tids) to the
 * controller over `report_fd`, fork the next level if any, then park in
 * pause() forever. Status to the controller: one record per member. */

struct member_record {
    int pid;
    int level;              /* 0 browser, 1 zygote, 2 renderer */
    int tids[WORKERS];
};

static void member(int level, int report_fd)
{
    static struct ctx c;
    pthread_t th[WORKERS];
    memset(&c, 0, sizeof c);
    if (start_workers(&c, th) < 0) {
        fprintf(stderr, "member level %d: workers failed: %s\n", level, strerror(errno));
        _exit(99);
    }
    while (c.ready < WORKERS) msleep(1);
    struct member_record r = { .pid = getpid(), .level = level };
    memcpy(r.tids, c.tids, sizeof r.tids);
    if (write(report_fd, &r, sizeof r) != sizeof r) _exit(98);
    if (level < 2) {
        pid_t k = fork();
        if (k == 0) member(level + 1, report_fd);
        if (k < 0) fprintf(stderr, "member level %d: fork: %s\n", level, strerror(errno));
    }
    close(report_fd);
    for (;;) pause();
}

/* ------------------------------------------------------------------------ */
/* The bystander: same workers, counts errors, reports the count when asked
 * with a byte on `cmd_fd`, exits on 'q'. */

static void bystander(int cmd_fd, int ans_fd)
{
    static struct ctx c;
    static volatile long spurious;
    pthread_t th[WORKERS];
    memset(&c, 0, sizeof c);
    c.spurious = &spurious;
    if (start_workers(&c, th) < 0) _exit(97);
    while (c.ready < WORKERS) msleep(1);
    for (;;) {
        char b;
        if (read(cmd_fd, &b, 1) != 1) _exit(0);
        long n = spurious;
        if (write(ans_fd, &n, sizeof n) != sizeof n) _exit(0);
        if (b == 'q') _exit(0);
    }
}

/* ------------------------------------------------------------------------ */
/* Controller side. */

/* Is `pid` still a live (non-zombie) process? */
static int alive(int pid)
{
    char path[64], buf[512];
    if (kill(pid, 0) < 0 && errno == ESRCH) return 0;
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = 0;
    char *s = strstr(buf, "State:");
    if (!s) return 1;
    s += 6;
    while (*s == ' ' || *s == '\t') s++;
    return !(*s == 'Z' || *s == 'X');
}

static long ask_bystander(int cmd_w, int ans_r, char cmd)
{
    long n = -1;
    if (write(cmd_w, &cmd, 1) != 1) return -1;
    if (read(ans_r, &n, sizeof n) != sizeof n) return -1;
    return n;
}

static int do_kill(int target, int sig, const char *what)
{
    if (kill(target, sig) < 0) {
        fprintf(stderr, "  kill(%s=%d, %d): %s\n", what, target, sig, strerror(errno));
        return -1;
    }
    return 0;
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 10;
    int mode_arg = argc > 2 ? atoi(argv[2]) : 0;
    int failed = 0;
    long worst = 0;

    setvbuf(stdout, NULL, _IOLBF, 0);
    /* Reap the whole tree ourselves where the kernel lets us (Linux). The
     * renderer is a grandchild; without this it goes to init on exit. On a
     * kernel without PR_SET_CHILD_SUBREAPER the probe still judges members
     * through /proc, so the verdict does not depend on it. */
    int subreaper = prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0;

    int bcmd[2], bans[2];
    if (pipe(bcmd) < 0 || pipe(bans) < 0) { perror("pipe"); return 2; }
    pid_t by = fork();
    if (by == 0) {
        setpgid(0, 0);
        close(bcmd[1]); close(bans[0]);
        bystander(bcmd[0], bans[1]);
    }
    close(bcmd[0]); close(bans[1]);
    msleep(SETTLE_MS);
    if (ask_bystander(bcmd[1], bans[0], 'p') != 0) {
        printf("FAIL: bystander did not come up clean\n");
        return 1;
    }
    printf("killtree: %d rounds, mode %d, %d workers/member, subreaper=%d\n",
           rounds, mode_arg, WORKERS, subreaper);

    for (int round = 0; round < rounds; round++) {
        int mode = mode_arg ? mode_arg : 1 + (round % 4);
        int rep[2];
        if (pipe(rep) < 0) { perror("pipe"); return 2; }
        pid_t browser = fork();
        if (browser == 0) {
            setpgid(0, 0);
            close(rep[0]);
            member(0, rep[1]);
        }
        if (browser < 0) { perror("fork"); return 2; }
        close(rep[1]);
        /* Three records: browser, zygote, renderer. */
        struct member_record recs[3];
        int nrec = 0;
        for (;;) {
            struct member_record r;
            ssize_t n = read(rep[0], &r, sizeof r);
            if (n != sizeof r) break;
            if (nrec < 3) recs[nrec++] = r;
        }
        close(rep[0]);
        if (nrec != 3) {
            printf("round %d: FAIL — only %d of 3 members reported\n", round, nrec);
            failed++;
            do_kill(-browser, SIGKILL, "pgid");
            do_kill(browser, SIGKILL, "browser");
            waitpid(browser, NULL, 0);
            continue;
        }
        msleep(SETTLE_MS);

        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        const char *how;
        long reap_ms = -1;
        int st = 0;
        switch (mode) {
        case 1:
            /* Linux: a parent's death does not touch its children, so the
             * zygote and renderer stay (verified: `left=2` for 10 s). Take
             * them after the browser is reaped, which is what kami's group
             * kill amounts to; the first kill is the one under test. */
            how = "SIGKILL browser, then the rest";
            do_kill(browser, SIGKILL, "browser");
            for (;;) {
                pid_t r = waitpid(browser, &st, WNOHANG);
                if (r == browser) { reap_ms = ms_since(&t0); break; }
                if (r < 0) { reap_ms = ms_since(&t0); st = -1; break; }
                if (ms_since(&t0) > LIMIT_MS) break;
                msleep(2);
            }
            do_kill(recs[1].pid, SIGKILL, "zygote");
            do_kill(recs[2].pid, SIGKILL, "renderer");
            break;
        case 2:
            how = "SIGKILL group";
            if (do_kill(-browser, SIGKILL, "pgid") < 0) {
                /* A kernel without group kill: fall back per pid so the
                 * round still exercises the hard path, and say so. */
                for (int i = 0; i < nrec; i++) do_kill(recs[i].pid, SIGKILL, "member");
            }
            break;
        case 3:
            how = "SIGTERM group, then SIGKILL";
            if (do_kill(-browser, SIGTERM, "pgid") < 0)
                for (int i = 0; i < nrec; i++) do_kill(recs[i].pid, SIGTERM, "member");
            msleep(300);
            if (do_kill(-browser, SIGKILL, "pgid") < 0)
                for (int i = 0; i < nrec; i++) do_kill(recs[i].pid, SIGKILL, "member");
            break;
        default: {
            /* `kill -9` addressed at a worker thread of the zygote: on Linux
             * a thread id is a valid kill() target and takes the whole group.
             * Only where tids and pids share a namespace: on Akuma/amd64
             * `gettid()` is the kernel task slot, a different number space,
             * so `kill(tid)` would land on whatever process has that pid —
             * the bystander, or this probe. Detected by the main thread's
             * own tid, which equals its pid on Linux. */
            int w = (round / 4) % WORKERS;
            if ((int)syscall(SYS_gettid) != getpid()) {
                how = "SIGKILL a worker tid (SKIPPED: tid != pid space)";
                do_kill(recs[1].pid, SIGKILL, "zygote");
            } else {
                how = "SIGKILL a zygote worker tid";
                if (do_kill(recs[1].tids[w], SIGKILL, WORK_NAME[w]) < 0)
                    do_kill(recs[1].pid, SIGKILL, "zygote");
            }
            /* and the rest of the tree, as kami would */
            do_kill(browser, SIGKILL, "browser");
            do_kill(recs[2].pid, SIGKILL, "renderer");
            break;
        }
        }

        /* Reap the browser (our child) with a deadline. */
        while (reap_ms < 0) {
            pid_t r = waitpid(browser, &st, WNOHANG);
            if (r == browser) { reap_ms = ms_since(&t0); break; }
            if (r < 0) { reap_ms = ms_since(&t0); st = -1; break; }
            if (ms_since(&t0) > LIMIT_MS) break;
            msleep(2);
        }
        /* Every member gone (zombie or absent), with a deadline. */
        int left = 0;
        for (;;) {
            left = 0;
            for (int i = 0; i < nrec; i++) left += alive(recs[i].pid);
            if (left == 0 || ms_since(&t0) > LIMIT_MS) break;
            msleep(2);
        }
        long tree_ms = ms_since(&t0);
        /* Collect orphans we can see (subreaper or direct children). */
        while (waitpid(-1, NULL, WNOHANG) > 0) {}

        long spur = ask_bystander(bcmd[1], bans[0], 'p');
        int ok = reap_ms >= 0 && left == 0 && spur == 0;
        if (!ok) failed++;
        if (tree_ms > worst) worst = tree_ms;
        printf("round %d: %-34s reap=%ld ms tree=%ld ms left=%d status=%s%d bystander_errs=%ld %s\n",
               round, how, reap_ms, tree_ms, left,
               WIFSIGNALED(st) ? "sig" : "exit",
               WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st),
               spur, ok ? "ok" : "FAIL");
        if (left) {
            for (int i = 0; i < nrec; i++)
                if (alive(recs[i].pid))
                    printf("  still alive: level %d pid %d\n", recs[i].level, recs[i].pid);
        }
    }

    long spur = ask_bystander(bcmd[1], bans[0], 'q');
    waitpid(by, NULL, 0);
    printf("worst tree teardown %ld ms, bystander errors %ld\n", worst, spur);
    if (failed || spur != 0) {
        printf("FAIL: %d of %d rounds\n", failed, rounds);
        return 1;
    }
    printf("PASS: %d rounds\n", rounds);
    return 0;
}
