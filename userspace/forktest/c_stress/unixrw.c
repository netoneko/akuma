/*
 * unixrw - read(2)/write(2) on an AF_UNIX socket must be recv/send with flags 0.
 *
 * Rust std's fork+exec spawn (the path cargo takes: jobserver's pre_exec rules
 * out posix_spawn) reports a failed exec over a SOCK_SEQPACKET socketpair: the
 * child write(2)s errno+"NOEX", the parent recv(2)s it. Akuma's glue served
 * read/write on a socket as raw pipe bytes, outside the record queue recv keys a
 * framed socket off, so recv never saw the message: it spun with the BKL held
 * (a wedged guest) or read the child's exit as EOF (the exec "succeeded", the
 * child exited 1, and cargo never took its E2BIG -> @argfile retry).
 * docs/archive/UNIX_SOCKET_READ_WRITE_BYPASSED_RECORDS.md
 *
 * Prints PASS/FAIL per probe; exit 0 only when all pass. Every probe is
 * bounded by alarm(), so a kernel that never delivers fails instead of hanging
 * (a kernel that spins with interrupts off still hangs - that is the wedge).
 *
 *   x86_64-linux-musl-gcc -static -O2 -Wall -Wextra -o unixrw unixrw.c
 */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;

static void report(const char *name, int ok, const char *detail)
{
    printf("%s %s%s%s\n", ok ? "PASS" : "FAIL", name, detail ? ": " : "", detail ? detail : "");
    if (!ok)
        fails++;
}

static void on_alarm(int sig) { (void)sig; }

/* Akuma's socketpair takes STREAM and SEQPACKET only; a refused DGRAM pair is a
 * known gap, not this probe's subject, so it is reported and not counted. */
static void skip_or_fail(const char *name, int type)
{
    char detail[64];
    snprintf(detail, sizeof detail, "socketpair errno=%d", errno);
    if (type == SOCK_DGRAM)
        printf("SKIP %s: %s (no SOCK_DGRAM socketpair here)\n", name, detail);
    else
        report(name, 0, detail);
}

/* write() on one end, recv() on the other: the message arrives whole. */
static void write_then_recv(int type, const char *name)
{
    int sv[2];
    char buf[32];
    char detail[64];
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, sv) != 0) {
        skip_or_fail(name, type);
        return;
    }
    ssize_t w = write(sv[1], "12345678", 8);
    alarm(5);
    ssize_t r = recv(sv[0], buf, sizeof buf, 0);
    alarm(0);
    snprintf(detail, sizeof detail, "write=%zd recv=%zd errno=%d", w, r, r < 0 ? errno : 0);
    report(name, w == 8 && r == 8 && memcmp(buf, "12345678", 8) == 0, detail);
    close(sv[0]);
    close(sv[1]);
}

/* send() on one end, read() on the other. */
static void send_then_read(int type, const char *name)
{
    int sv[2];
    char buf[32];
    char detail[64];
    if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, sv) != 0) {
        skip_or_fail(name, type);
        return;
    }
    ssize_t w = send(sv[1], "abcdefgh", 8, 0);
    alarm(5);
    ssize_t r = read(sv[0], buf, sizeof buf);
    alarm(0);
    snprintf(detail, sizeof detail, "send=%zd read=%zd errno=%d", w, r, r < 0 ? errno : 0);
    report(name, w == 8 && r == 8 && memcmp(buf, "abcdefgh", 8) == 0, detail);
    close(sv[0]);
    close(sv[1]);
}

/* Two write()s on a SEQPACKET pair are two records: a big read() returns the first only. */
static void seqpacket_write_boundaries(void)
{
    int sv[2];
    char buf[64];
    char detail[64];
    if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) != 0) {
        report("seqpacket write keeps record boundaries", 0, "socketpair failed");
        return;
    }
    (void)!write(sv[1], "first", 5);
    (void)!write(sv[1], "second", 6);
    alarm(5);
    ssize_t a = read(sv[0], buf, sizeof buf);
    ssize_t b = read(sv[0], buf + 8, sizeof buf - 8);
    alarm(0);
    snprintf(detail, sizeof detail, "reads=%zd,%zd (want 5,6)", a, b);
    report("seqpacket write keeps record boundaries", a == 5 && b == 6, detail);
    close(sv[0]);
    close(sv[1]);
}

/*
 * std's exact shape: SEQPACKET pair, fork, the child execve()s a missing path,
 * write()s errno+"NOEX" and _exit(1)s; the parent closes its copy of the
 * child's end and recv()s 8 bytes. Repeated, because whether the child or the
 * parent reaches the socket first decided which of the two failures showed.
 */
static void std_spawn_error_channel(void)
{
    char detail[96];
    int got = 0, eof = 0, other = 0;
    for (int i = 0; i < 20; i++) {
        int sv[2];
        if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) != 0) {
            other++;
            continue;
        }
        pid_t pid = fork();
        if (pid == 0) {
            close(sv[0]);
            char *argv[] = {"/nonexistent/unixrw", 0};
            char *envp[] = {0};
            execve(argv[0], argv, envp);
            unsigned char msg[8] = {0, 0, 0, (unsigned char)errno, 'N', 'O', 'E', 'X'};
            (void)!write(sv[1], msg, 8);
            _exit(1);
        }
        close(sv[1]);
        unsigned char buf[8];
        alarm(5);
        ssize_t r = recv(sv[0], buf, 8, 0);
        alarm(0);
        if (r == 8 && memcmp(buf + 4, "NOEX", 4) == 0 && buf[3] == ENOENT)
            got++;
        else if (r == 0)
            eof++;
        else
            other++;
        close(sv[0]);
        waitpid(pid, 0, 0);
    }
    snprintf(detail, sizeof detail, "errno delivered %d/20, EOF %d, other %d", got, eof, other);
    report("std spawn error channel (fork, failed execve, write, recv)", got == 20, detail);
}

int main(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm; /* no SA_RESTART: the blocked call returns EINTR */
    sigaction(SIGALRM, &sa, 0);

    write_then_recv(SOCK_SEQPACKET, "seqpacket write -> recv");
    write_then_recv(SOCK_DGRAM, "dgram write -> recv");
    write_then_recv(SOCK_STREAM, "stream write -> recv");
    send_then_read(SOCK_SEQPACKET, "seqpacket send -> read");
    send_then_read(SOCK_DGRAM, "dgram send -> read");
    send_then_read(SOCK_STREAM, "stream send -> read");
    seqpacket_write_boundaries();
    std_spawn_error_channel();
    printf("%s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
