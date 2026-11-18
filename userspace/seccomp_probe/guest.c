/* SPDX-License-Identifier: BSD-2-Clause */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#if defined(__x86_64__)
enum { NR_READ = 0, NR_SOCKET = 41, NR_GETPID = 39, NR_SIGRETURN = 15,
       NR_PRCTL = 157, NR_SECCOMP = 317 };
#define AUDIT_ARCH UINT32_C(0xc000003e)
#elif defined(__aarch64__)
enum { NR_READ = 63, NR_SOCKET = 198, NR_GETPID = 172, NR_SIGRETURN = 139,
       NR_PRCTL = 167, NR_SECCOMP = 277 };
#define AUDIT_ARCH UINT32_C(0xc00000b7)
#else
#error unsupported probe architecture
#endif

enum {
    GET_SECCOMP = 21, SET_SECCOMP, SET_NO_NEW_PRIVS = 38, GET_NO_NEW_PRIVS,
    MODE_FILTER = 2, SET_MODE_FILTER = 1, GET_ACTION_AVAIL = 2,
    LOAD_ABS = 0x20, JEQ = 0x15, RET = 6,
    KILL_THREAD = 0, TRAP = 0x00030000, RETURN_ERRNO = 0x00050000,
    TRACE = 0x7ff00000, ALLOW = 0x7fff0000
};
#define KILL_PROCESS UINT32_C(0x80000000)

struct insn {
    uint16_t code;
    uint8_t jt, jf;
    uint32_t k;
};

struct fprog {
    uint16_t len;
    const struct insn *filter;
};

static const struct insn socket_filter[] = {
    {LOAD_ABS, 0, 0, 4},
    {JEQ, 1, 0, AUDIT_ARCH},
    {RET, 0, 0, KILL_PROCESS},
    {LOAD_ABS, 0, 0, 0},
    {JEQ, 0, 1, NR_SOCKET},
    {RET, 0, 0, RETURN_ERRNO | EPERM},
    {RET, 0, 0, ALLOW},
};

#define COUNT(a) (sizeof(a) / sizeof((a)[0]))

static void check(int condition, const char *what) {
    if (!condition) {
        fprintf(stderr, "FAIL %s (errno=%d)\n", what, errno);
        exit(1);
    }
}

static int install(const struct insn *filter, uint16_t len, int via_prctl) {
    struct fprog f = {len, filter};
    if (via_prctl)
        return syscall(NR_PRCTL, SET_SECCOMP, MODE_FILTER, &f, 0, 0);
    return syscall(NR_SECCOMP, SET_MODE_FILTER, 0, &f);
}

static void filter_one(uint32_t nr, uint32_t action) {
    struct insn filter[] = {
        {LOAD_ABS, 0, 0, 0},
        {JEQ, 0, 1, nr},
        {RET, 0, 0, action},
        {RET, 0, 0, ALLOW},
    };
    check(install(filter, COUNT(filter), 1) == 0, "PR_SET_SECCOMP");
}

static void filtered_io(int expected_errno) {
    int fd = open("/dev/null", O_RDONLY);
    char ch;
    check(fd >= 0 && syscall(NR_READ, fd, &ch, 1) == 0, "read allowed");
    close(fd);
    check(syscall(NR_SOCKET, 2, 1, 0) == -1 && errno == expected_errno, "socket denied");
    check(syscall(NR_PRCTL, GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1, "sticky NO_NEW_PRIVS");
    check(syscall(NR_PRCTL, GET_SECCOMP, 0, 0, 0, 0) == MODE_FILTER, "seccomp mode");
}

static volatile sig_atomic_t trapped;
static void on_sigsys(int signal, siginfo_t *info, void *context) {
    (void)context;
    trapped = signal == SIGSYS && info->si_code == 1 && info->si_errno == 42
              && info->si_syscall == NR_GETPID && info->si_arch == AUDIT_ARCH
              && info->si_call_addr != NULL;
}

static void *clone_check(void *unused) {
    (void)unused;
    filtered_io(EPERM);
    filter_one(NR_SOCKET, RETURN_ERRNO | EBUSY);
    filtered_io(EBUSY);
    return NULL;
}

static void *surviving_thread(void *arg) {
    int *fds = arg;
    char byte;
    if (read(fds[0], &byte, 1) != 1)
        return NULL;
    usleep(50 * 1000);
    if (write(fds[1], "s", 1) != 1)
        _exit(1);
    return NULL;
}

static void test_leader_kill_thread(void) {
    int result[2], start[2];
    check(pipe(result) == 0 && pipe(start) == 0, "leader test pipes");
    pid_t child = fork();
    check(child >= 0, "leader test fork");
    if (child == 0) {
        close(result[0]);
        int worker_fds[] = {start[0], result[1]};
        pthread_t worker;
        check(pthread_create(&worker, NULL, surviving_thread, worker_fds) == 0, "surviving worker");
        filter_one(NR_GETPID, KILL_THREAD);
        check(write(start[1], "!", 1) == 1, "wake surviving worker");
        syscall(NR_GETPID);
        _exit(1);
    }
    close(result[1]);
    close(start[0]);
    close(start[1]);
    int status;
    check(waitpid(child, &status, 0) == child, "wait for killed leader");
    char b;
    check(read(result[0], &b, 1) == 1 && b == 's', "KILL_THREAD preserves sibling");
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "surviving group status");
    close(result[0]);
}

static void test_action(uint32_t action, int expected_signal) {
    pid_t child = fork();
    check(child >= 0, "fork action test");
    if (child == 0) {
        if (action == (TRAP | 42)) {
            struct sigaction sa = { .sa_sigaction = on_sigsys, .sa_flags = SA_SIGINFO };
            check(sigaction(SIGSYS, &sa, NULL) == 0, "SIGSYS handler");
        }
        filter_one(NR_GETPID, action);
        long ret = syscall(NR_GETPID);
        if (action == (TRAP | 42))
            _exit(trapped ? 0 : 1);
        if (action == TRACE)
            _exit(ret == -1 && errno == ENOSYS ? 0 : 1);
        _exit(1);
    }
    int status;
    check(waitpid(child, &status, 0) == child, "wait for action");
    if (expected_signal != 0)
        check(WIFSIGNALED(status) && WTERMSIG(status) == expected_signal, "SIGSYS wait status");
    else
        check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "action behavior");
}

static void drop_root(void) {
    if (geteuid() == 0)
        check(setgid(65534) == 0 && setuid(65534) == 0 && geteuid() == 65534, "drop root");
}

int main(int argc, char **argv) {
    if (argc >= 4 && strcmp(argv[1], "--exec-unprivileged") == 0) {
        drop_root();
        execv(argv[2], &argv[3]);
        perror("exec");
        return 1;
    }
    if (argc == 2 && strcmp(argv[1], "--exec-child") == 0) {
        filtered_io(EPERM);
        puts("exec: inherited seccomp and NO_NEW_PRIVS");
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "--codex-child") == 0) {
        filtered_io(EPERM);
        puts("Codex filter active after exec");
        return 0;
    }
    setvbuf(stdout, NULL, _IONBF, 0);
    drop_root();
    check(syscall(NR_PRCTL, GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 0, "initial NO_NEW_PRIVS");
    check(install(socket_filter, COUNT(socket_filter), 0) == -1 && errno == EACCES,
          "unprivileged install without NO_NEW_PRIVS");
    puts("unprivileged install: EACCES");
    check(syscall(NR_PRCTL, SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0, "PR_SET_NO_NEW_PRIVS");
    check(syscall(NR_PRCTL, SET_NO_NEW_PRIVS, 0, 0, 0, 0) == -1 && errno == EINVAL,
          "cannot clear NO_NEW_PRIVS");
    uint32_t action = KILL_PROCESS;
    check(syscall(NR_SECCOMP, GET_ACTION_AVAIL, 0, &action) == 0, "GET_ACTION_AVAIL");
    struct insn invalid = {0xffff, 0, 0, 0};
    check(install(&invalid, 1, 0) == -1 && errno == EINVAL, "reject invalid opcode");
    test_action(TRAP | 42, 0);
    test_action(TRACE, 0);
    test_action(KILL_THREAD, SIGSYS);
    test_action(KILL_PROCESS, SIGSYS);
    test_leader_kill_thread();
    check(install(socket_filter, COUNT(socket_filter), 0) == 0, "SECCOMP_SET_MODE_FILTER");
    filtered_io(EPERM);
    puts("installed filter: socket EPERM, read allowed");
    pthread_t worker;
    check(pthread_create(&worker, NULL, clone_check, NULL) == 0
          && pthread_join(worker, NULL) == 0, "CLONE_THREAD inherits policy");
    filtered_io(EPERM);
    puts("clone: inherited policy, worker's new filter stayed local");

    pid_t child = fork();
    check(child >= 0, "fork");
    if (child == 0) {
        filtered_io(EPERM);
        puts("fork: inherited seccomp and NO_NEW_PRIVS");
        fflush(stdout);
        execl(argv[0], argv[0], "--exec-child", (char *)NULL);
        _exit(1);
    }
    int status;
    check(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "exec child succeeded");
    filter_one(NR_SOCKET, RETURN_ERRNO | EBUSY);
    filtered_io(EBUSY);
    filter_one(NR_SIGRETURN, RETURN_ERRNO | EPERM);
    check(syscall(NR_SIGRETURN) == -1 && errno == EPERM, "rt_sigreturn filtered");
    puts("stacked filter: newest errno wins, rt_sigreturn denied");
    return 0;
}
