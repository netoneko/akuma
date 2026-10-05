/* ptyprobe — Unix98 pseudo-terminals (/dev/ptmx + /dev/pts/N), end to end.
 *
 * Added 2026-10-05 with the kernel's pty support (`crates/akuma-pty`,
 * `akuma-syscalls-glue/src/pty.rs`). Before that `openpty` failed and every
 * terminal emulator (rio) fell back to running its shell on pipes. Every check
 * prints PASS/FAIL; the last line is "RESULT: all passed" or a failure count.
 *
 * Drives a real `sh -i` on the slave from the master the way a terminal
 * emulator does: line discipline and echo, `stty size`, `tty`, `/dev/tty`,
 * SIGWINCH on resize, ^C reaching the foreground job, edge-triggered epoll on
 * the master, and the hangup rules in both directions.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -o ptyprobe ptyprobe.c
 * Run:   /tmp/ptyprobe            (it re-executes itself as `ptyprobe winch`,
 *                                  so run it by an absolute path)              */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <fcntl.h>
#include <poll.h>
#include <pty.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int fails;
#define CHECK(c, ...) do { if (c) { printf("PASS "); printf(__VA_ARGS__); printf("\n"); } \
    else { printf("FAIL "); printf(__VA_ARGS__); printf(" (errno=%d)\n", errno); fails++; } fflush(stdout); } while (0)

static long now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000L + t.tv_nsec / 1000000L;
}

/* Everything the master has produced so far. */
static char out[65536];
static size_t outlen;

/* Read from the (non-blocking) master until `needle` shows up after offset
 * `from`, or `ms` passes. Returns 1 if found. */
static int expect(int m, size_t from, const char *needle, int ms) {
    long end = now_ms() + ms;
    for (;;) {
        out[outlen] = 0;
        if (outlen > from && strstr(out + from, needle))
            return 1;
        long left = end - now_ms();
        if (left <= 0)
            return 0;
        struct pollfd p = { .fd = m, .events = POLLIN };
        poll(&p, 1, left > 100 ? 100 : (int)left);
        ssize_t n = read(m, out + outlen, sizeof out - 1 - outlen);
        if (n > 0)
            outlen += (size_t)n;
        else if (n < 0 && errno == EIO)
            return 0;
    }
}

static void say(int m, const char *s) { (void)!write(m, s, strlen(s)); }

static void dump_tail(void) {
    size_t from = outlen > 400 ? outlen - 400 : 0;
    printf("  --- master output tail ---\n  ");
    for (size_t i = from; i < outlen; i++) {
        unsigned char c = (unsigned char)out[i];
        if (c == '\n') printf("\\n\n  ");
        else if (c == '\r') printf("\\r");
        else if (c < 32 || c == 127) printf("^%c", c ^ 0x40);
        else putchar(c);
    }
    printf("\n  ---\n");
}

static volatile sig_atomic_t winched;
static void on_winch(int s) { (void)s; winched = 1; }

/* `ptyprobe winch`: run on the pty by the shell. Print READY, wait for one
 * SIGWINCH, report the size the slave now has. */
static int winch_child(void) {
    signal(SIGWINCH, on_winch);
    printf("READY\n");
    fflush(stdout);
    for (int i = 0; i < 100 && !winched; i++)
        usleep(50000);
    struct winsize ws = {0};
    ioctl(0, TIOCGWINSZ, &ws);
    printf(winched ? "WINCH %dx%d\n" : "NOWINCH %dx%d\n", ws.ws_row, ws.ws_col);
    return 0;
}

/* Is `name` an entry of directory `dir`? Reports its d_type through `type`. */
static int listed(const char *dir, const char *name, int *type) {
    DIR *d = opendir(dir);
    if (!d) return 0;
    struct dirent *e;
    int found = 0;
    while ((e = readdir(d)))
        if (!strcmp(e->d_name, name)) { found = 1; if (type) *type = e->d_type; }
    closedir(d);
    return found;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "winch"))
        return winch_child();
    setvbuf(stdout, NULL, _IONBF, 0);

    /* ---- the nodes ----------------------------------------------------------- */
    struct stat st0;
    int ty = -1;
    CHECK(stat("/dev/ptmx", &st0) == 0 && S_ISCHR(st0.st_mode) && major(st0.st_rdev) == 5 && minor(st0.st_rdev) == 2,
          "stat /dev/ptmx is char 5:2");
    CHECK(listed("/dev", "ptmx", NULL), "ls /dev shows ptmx");
    CHECK(listed("/dev", "pts", &ty) && ty == DT_DIR, "ls /dev shows pts/ as a directory");
    CHECK(stat("/dev/pts", &st0) == 0 && S_ISDIR(st0.st_mode), "stat /dev/pts is a directory");

    /* ---- the device without a shell -------------------------------------- */
    int pm = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(pm >= 0, "posix_openpt");
    char *pn = ptsname(pm);
    CHECK(pn && !strncmp(pn, "/dev/pts/", 9), "ptsname = %s", pn ? pn : "(null)");
    errno = 0;
    int ps = open(pn, O_RDWR | O_NOCTTY);
    CHECK(ps < 0 && errno == EIO, "slave open before unlockpt is EIO");
    CHECK(unlockpt(pm) == 0, "unlockpt");
    ps = open(pn, O_RDWR | O_NOCTTY);
    CHECK(ps >= 0, "slave opens after unlockpt");
    const char *num = pn + 9;
    ty = -1;
    CHECK(listed("/dev/pts", num, &ty) && ty == DT_CHR, "ls /dev/pts shows %s as a char device", num);
    struct stat sst;
    CHECK(stat(pn, &sst) == 0 && S_ISCHR(sst.st_mode) && major(sst.st_rdev) == 136, "stat %s is char 136:N", pn);
    char numcopy[16];
    snprintf(numcopy, sizeof numcopy, "%s", num);
    close(pm);
    char b[16];
    CHECK(read(ps, b, sizeof b) == 0, "slave reads EOF once the master is closed");
    errno = 0;
    CHECK(write(ps, "x", 1) < 0 && errno == EIO, "slave write is EIO once the master is closed");
    close(ps);
    CHECK(!listed("/dev/pts", numcopy, NULL), "/dev/pts/%s is gone once both sides are closed", numcopy);

    int m, s;
    char name[64];
    CHECK(openpty(&m, &s, name, NULL, NULL) == 0, "openpty -> %s", name);
    CHECK(isatty(s) && isatty(m), "isatty on both sides");
    char *tn = ttyname(s);
    CHECK(tn && !strcmp(tn, name), "ttyname(slave) = %s", tn ? tn : "(null)");
    struct winsize ws = {0};
    CHECK(ioctl(s, TIOCGWINSZ, &ws) == 0 && ws.ws_row == 24 && ws.ws_col == 80, "initial size 24x80 (%dx%d)", ws.ws_row, ws.ws_col);
    struct termios t;
    CHECK(tcgetattr(s, &t) == 0 && (t.c_lflag & ICANON) && (t.c_lflag & ECHO) && t.c_cc[VINTR] == 3 && t.c_cc[VERASE] == 0177 && t.c_cc[VMIN] == 1,
          "tcgetattr: cooked, VINTR=^C, VERASE=DEL, VMIN=1 at the right offsets");
    fcntl(m, F_SETFL, fcntl(m, F_GETFL) | O_NONBLOCK);
    errno = 0;
    CHECK(read(m, b, sizeof b) < 0 && errno == EAGAIN, "non-blocking master read with nothing queued is EAGAIN");

    say(m, "abc\r");
    char line[32] = {0};
    ssize_t n = read(s, line, sizeof line - 1);
    CHECK(n == 4 && !memcmp(line, "abc\n", 4), "cooked: master \"abc\\r\" reads as \"abc\\n\" on the slave");
    CHECK(expect(m, 0, "abc\r\n", 1000), "echo comes back on the master with ONLCR");
    outlen = 0;

    ws.ws_row = 50; ws.ws_col = 132;
    CHECK(ioctl(m, TIOCSWINSZ, &ws) == 0, "TIOCSWINSZ on the master");
    struct winsize ws2 = {0};
    CHECK(ioctl(s, TIOCGWINSZ, &ws2) == 0 && ws2.ws_row == 50 && ws2.ws_col == 132, "slave sees 50x132");

    /* ---- a shell on the slave ---------------------------------------------- */
    pid_t sh = fork();
    if (sh == 0) {
        close(m);
        setsid();
        if (ioctl(s, TIOCSCTTY, 0) < 0)
            _exit(90);
        dup2(s, 0); dup2(s, 1); dup2(s, 2);
        if (s > 2)
            close(s);
        setenv("PS1", "PTY$ ", 1);
        execl("/bin/sh", "sh", "-i", (char *)NULL);
        _exit(91);
    }
    close(s);
    CHECK(expect(m, 0, "PTY$ ", 5000), "interactive shell prompt appears");

    size_t mark = outlen;
    say(m, "echo XY$((40+2))Z\n");
    CHECK(expect(m, mark, "XY42Z", 3000), "command output comes back (echo XY$((40+2))Z)");

    mark = outlen;
    say(m, "stty size\n");
    CHECK(expect(m, mark, "50 132", 3000), "stty size inside the shell = 50 132");

    mark = outlen;
    say(m, "tty\n");
    CHECK(expect(m, mark, name, 3000), "tty inside the shell = %s", name);

    mark = outlen;
    say(m, "echo TT$((3*3)) > /dev/tty\n");
    CHECK(expect(m, mark, "TT9", 3000), "/dev/tty is the pty inside its session");

    /* ash turns job control (`-m`) on only if `tcgetpgrp() == getpgrp()`
     * works out on its terminal; otherwise it prints "job control turned off". */
    mark = outlen;
    say(m, "case $- in *m*) echo JC$((5+5));; esac\n");
    CHECK(expect(m, mark, "JC10", 3000) && !strstr(out, "job control turned off"),
          "the shell got job control on its terminal (set -m)");

    /* SIGWINCH reaches the foreground job. */
    mark = outlen;
    char cmd[300];
    snprintf(cmd, sizeof cmd, "%s winch\n", argv[0]);
    say(m, cmd);
    if (expect(m, mark, "READY", 5000)) {
        ws.ws_row = 40; ws.ws_col = 100;
        ioctl(m, TIOCSWINSZ, &ws);
        CHECK(expect(m, mark, "WINCH 40x100", 6000), "resize sends SIGWINCH to the foreground job");
    } else {
        CHECK(0, "winch child started (needs an absolute argv[0])");
    }
    expect(m, outlen, "PTY$ ", 3000);

    /* ^C kills the foreground job, not the shell. */
    mark = outlen;
    long t0 = now_ms();
    say(m, "sleep 20\n");
    usleep(700000);
    say(m, "\003");
    say(m, "echo DONE$((2+3))\n");
    int done = expect(m, mark, "DONE5", 8000);
    CHECK(done && now_ms() - t0 < 9000, "^C interrupts `sleep 20` and the shell carries on (%ld ms)", now_ms() - t0);

    /* Edge-triggered epoll on the master: one edge per arrival, re-armed by draining. */
    int ep = epoll_create1(0);
    struct epoll_event ev = { .events = EPOLLIN | EPOLLET, .data.fd = m };
    CHECK(ep >= 0 && epoll_ctl(ep, EPOLL_CTL_ADD, m, &ev) == 0, "epoll_ctl(ADD, master, EPOLLIN|EPOLLET)");
    expect(m, outlen, "\x01\x02never", 300); /* drain to EAGAIN */
    for (int round = 1; round <= 2; round++) {
        mark = outlen;
        snprintf(cmd, sizeof cmd, "echo EDGE$((%d*11))\n", round);
        say(m, cmd);
        struct epoll_event got;
        int r = epoll_wait(ep, &got, 1, 3000);
        char want[16];
        snprintf(want, sizeof want, "EDGE%d", round * 11);
        CHECK(r == 1 && (got.events & EPOLLIN), "epoll edge %d fires", round);
        expect(m, mark, want, 2000);
        expect(m, outlen, "\x01\x02never", 300);
    }
    close(ep);

    /* Shell exits: master reads EIO and polls POLLHUP; the shell is reaped. */
    say(m, "exit\n");
    long end = now_ms() + 5000;
    int eio = 0;
    while (now_ms() < end) {
        struct pollfd p = { .fd = m, .events = POLLIN };
        poll(&p, 1, 200);
        ssize_t r = read(m, b, sizeof b);
        if (r < 0 && errno == EIO) { eio = 1; break; }
    }
    CHECK(eio, "master read is EIO after the shell exits");
    struct pollfd p = { .fd = m, .events = POLLIN };
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP), "master polls POLLHUP (revents=0x%x)", p.revents);
    int st = 0;
    CHECK(waitpid(sh, &st, 0) == sh && WIFEXITED(st), "shell exited (status 0x%x)", st);
    close(m);

    if (fails)
        dump_tail();
    printf(fails ? "RESULT: %d failed\n" : "RESULT: all passed\n", fails);
    return fails ? 1 : 0;
}
