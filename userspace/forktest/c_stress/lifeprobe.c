/* lifeprobe — process-lifecycle checks from AKUMA_AMD64_WGPU_KERNEL_WORK.md §2.3.
 *
 * 1. Close-on-exec survives fork and is honoured by exec, for every way a
 *    descriptor can be created with it: socketpair(SOCK_CLOEXEC),
 *    socket(AF_UNIX, SOCK_CLOEXEC), pipe2(O_CLOEXEC), open(O_CLOEXEC),
 *    fcntl(F_DUPFD_CLOEXEC), dup3(O_CLOEXEC), fcntl(F_SETFD, FD_CLOEXEC),
 *    posix_openpt(O_CLOEXEC). rio's sockets showed up as fds 4/5 in every shell.
 * 2. SIGCHLD reaches a handler when a child exits.
 * 3. An orphan (its parent exited first) is reaped by someone — it must not
 *    stay a zombie forever.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -o lifeprobe lifeprobe.c
 * Run:   /tmp/lifeprobe   (absolute path: it re-executes itself)            */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int fails;
#define CHECK(c, ...) do { if (c) { printf("PASS "); printf(__VA_ARGS__); printf("\n"); } \
    else { printf("FAIL "); printf(__VA_ARGS__); printf(" (errno=%d)\n", errno); fails++; } fflush(stdout); } while (0)

/* `lifeprobe list W a b c ...`: write (as an int, to fd W — a plain pipe that
 * must itself survive exec) a bitmask of which of the named fds are still open. */
static int list_child(int argc, char **argv) {
    int mask = 0;
    for (int i = 3; i < argc; i++)
        if (fcntl(atoi(argv[i]), F_GETFD) >= 0)
            mask |= 1 << (i - 3);
    return write(atoi(argv[2]), &mask, sizeof mask) == sizeof mask ? 0 : 1;
}

static volatile sig_atomic_t chld;
static void on_chld(int s) { (void)s; chld++; }

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "list"))
        return list_child(argc, argv);
    setvbuf(stdout, NULL, _IONBF, 0);

    /* ---- 1. close-on-exec ------------------------------------------------- */
    struct { const char *how; int fd; } fds[16];
    int n = 0;
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) == 0) {
        fds[n].how = "socketpair(SOCK_CLOEXEC)[0]"; fds[n++].fd = sv[0];
        fds[n].how = "socketpair(SOCK_CLOEXEC)[1]"; fds[n++].fd = sv[1];
    } else CHECK(0, "socketpair");
    int us = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (us >= 0) { fds[n].how = "socket(AF_UNIX, SOCK_CLOEXEC)"; fds[n++].fd = us; }
    int pp[2];
    if (pipe2(pp, O_CLOEXEC) == 0) { fds[n].how = "pipe2(O_CLOEXEC)"; fds[n++].fd = pp[0]; }
    int of = open("/dev/null", O_RDONLY | O_CLOEXEC);
    if (of >= 0) { fds[n].how = "open(O_CLOEXEC)"; fds[n++].fd = of; }
    int plain = open("/dev/null", O_RDONLY);
    int dc = fcntl(plain, F_DUPFD_CLOEXEC, 20);
    if (dc >= 0) { fds[n].how = "fcntl(F_DUPFD_CLOEXEC)"; fds[n++].fd = dc; }
    int d3 = 30;
    if (dup3(plain, d3, O_CLOEXEC) == d3) { fds[n].how = "dup3(O_CLOEXEC)"; fds[n++].fd = d3; }
    int sf = open("/dev/null", O_RDONLY);
    if (fcntl(sf, F_SETFD, FD_CLOEXEC) == 0) { fds[n].how = "fcntl(F_SETFD, FD_CLOEXEC)"; fds[n++].fd = sf; }
    int pt = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (pt >= 0) { fds[n].how = "posix_openpt(O_CLOEXEC)"; fds[n++].fd = pt; }
    /* And one that must survive: the pipe the child reports through. */
    int rep[2];
    pipe(rep);

    char *args[24];
    char nums[18][12];
    int a = 0;
    args[a++] = argv[0];
    args[a++] = "list";
    snprintf(nums[n], sizeof nums[n], "%d", rep[1]); args[a++] = nums[n];
    for (int i = 0; i < n; i++) { snprintf(nums[i], sizeof nums[i], "%d", fds[i].fd); args[a++] = nums[i]; }
    args[a] = NULL;

    pid_t c = fork();
    if (c == 0) {
        execv(argv[0], args);
        _exit(255);
    }
    int st = 0;
    waitpid(c, &st, 0);
    close(rep[1]);
    int mask = -1;
    int got = (int)read(rep[0], &mask, sizeof mask);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0 && got == sizeof mask,
          "fork+exec of self reported back through a plain pipe, which survived exec (status 0x%x)", st);
    if (got == sizeof mask)
        for (int i = 0; i < n; i++)
            CHECK(!(mask & (1 << i)), "%s (fd %d) closed across exec", fds[i].how, fds[i].fd);

    /* ---- 2. SIGCHLD ---------------------------------------------------------- */
    struct sigaction sa = { .sa_handler = on_chld };
    sigemptyset(&sa.sa_mask);
    sigaction(SIGCHLD, &sa, NULL);
    for (int round = 0; round < 5; round++) {
        int before = chld;
        pid_t k = fork();
        if (k == 0)
            _exit(round);
        for (int i = 0; i < 200 && chld == before; i++)
            usleep(10000);
        CHECK(chld > before, "SIGCHLD handler ran for child exit %d", round);
        waitpid(k, NULL, 0);
    }
    signal(SIGCHLD, SIG_DFL);

    /* ---- 3. orphans ------------------------------------------------------------ */
    int gp[2];
    pipe(gp);
    pid_t mid = fork();
    if (mid == 0) {
        pid_t g = fork();
        if (g == 0) {
            usleep(300000); /* outlive the parent */
            _exit(0);
        }
        (void)!write(gp[1], &g, sizeof g);
        _exit(0);
    }
    pid_t orphan = 0;
    (void)!read(gp[0], &orphan, sizeof orphan);
    waitpid(mid, NULL, 0);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/stat", orphan);
    int gone = 0;
    char state = '?';
    for (int i = 0; i < 300 && !gone; i++) {
        FILE *f = fopen(path, "r");
        if (!f) { gone = 1; break; }
        char buf[512] = {0};
        (void)!fread(buf, 1, sizeof buf - 1, f);
        fclose(f);
        char *rp = strrchr(buf, ')');
        state = rp && rp[1] ? rp[2] : '?';
        usleep(10000);
    }
    CHECK(gone || getpid() == 1, "orphan %d was reaped (last state %c)", orphan, state);
    /* As pid 1 this probe is the reaper and does not wait: the orphan must
     * then at least *report* what it is — a zombie, not a running process. */
    if (!gone)
        CHECK(state == 'Z', "an unreaped exited orphan reports state Z (got %c)", state);
    if (!gone) {
        snprintf(path, sizeof path, "/proc/%d/status", orphan);
        FILE *f = fopen(path, "r");
        char line[128];
        while (f && fgets(line, sizeof line, f))
            if (!strncmp(line, "State", 5) || !strncmp(line, "PPid", 4) || !strncmp(line, "Name", 4))
                printf("  orphan %s", line);
        if (f) fclose(f);
        printf("  (this probe is pid %d; its parent is %d)\n", getpid(), getppid());
    }

    /* ---- 4. an orphan whose parent was SIGKILLed (rio, killed from ssh) ---- */
    if (getpid() != 1) {
        int kp[2];
        pipe(kp);
        pid_t victim = fork();
        if (victim == 0) {
            pid_t g = fork();
            if (g == 0) {
                usleep(400000);
                _exit(0);
            }
            (void)!write(kp[1], &g, sizeof g);
            for (;;)
                pause();
        }
        pid_t g2 = 0;
        (void)!read(kp[0], &g2, sizeof g2);
        kill(victim, SIGKILL);
        int vst = 0;
        CHECK(waitpid(victim, &vst, 0) == victim && WIFSIGNALED(vst) && WTERMSIG(vst) == SIGKILL,
              "SIGKILLed parent is reaped as killed (status 0x%x)", vst);
        snprintf(path, sizeof path, "/proc/%d/status", g2);
        int reparented = 0, reaped = 0;
        for (int i = 0; i < 300 && !reaped; i++) {
            FILE *f = fopen(path, "r");
            if (!f) { reaped = 1; break; }
            char line[128];
            while (fgets(line, sizeof line, f))
                if (!strncmp(line, "PPid:", 5) && atoi(line + 5) == 1)
                    reparented = 1;
            fclose(f);
            usleep(10000);
        }
        CHECK(reaped, "orphan %d of a SIGKILLed parent was reaped (seen reparented to 1: %d)", g2, reparented);
    }

    /* ---- 5. the console's non-canonical VMIN=0 reads ----------------------- */
    char link[64] = {0};
    /* Only meaningful on an idle console: an ssh session's stdin is at EOF,
     * and an EOF read returns 0 at once whatever VMIN/VTIME say. */
    struct pollfd idle = { .fd = 0, .events = POLLIN };
    int console = readlink("/proc/self/fd/0", link, sizeof link - 1) > 0 && !strcmp(link, "/dev/stdin")
                  && poll(&idle, 1, 0) == 0;
    if (console) {
        struct termios saved, raw;
        tcgetattr(0, &saved);
        raw = saved;
        raw.c_lflag &= ~(ICANON | ECHO);
        raw.c_cc[VMIN] = 0;
        raw.c_cc[VTIME] = 0;
        tcsetattr(0, TCSANOW, &raw);
        struct timespec t0, t1;
        char b;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        ssize_t r = read(0, &b, 1);
        clock_gettime(CLOCK_MONOTONIC, &t1);
        long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
        CHECK(r == 0 && ms < 100, "console VMIN=0 VTIME=0 read returns 0 at once (r=%zd, %ld ms)", r, ms);
        raw.c_cc[VTIME] = 3;
        tcsetattr(0, TCSANOW, &raw);
        clock_gettime(CLOCK_MONOTONIC, &t0);
        r = read(0, &b, 1);
        clock_gettime(CLOCK_MONOTONIC, &t1);
        ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
        CHECK(r == 0 && ms >= 250 && ms < 2000, "console VMIN=0 VTIME=3 read times out in ~300 ms (r=%zd, %ld ms)", r, ms);
        tcsetattr(0, TCSANOW, &saved);
        struct termios back;
        tcgetattr(0, &back);
        CHECK(back.c_cc[VMIN] == saved.c_cc[VMIN] && back.c_cc[VINTR] == 3, "console termios round-trips (VMIN=%d VINTR=%d)", back.c_cc[VMIN], back.c_cc[VINTR]);
    } else {
        printf("SKIP console VMIN/VTIME: fd 0 is %s and %s\n", link[0] ? link : "?", idle.revents ? "readable/at EOF" : "not the console");
    }

    printf(fails ? "RESULT: %d failed\n" : "RESULT: all passed\n", fails);
    return fails ? 1 : 0;
}
