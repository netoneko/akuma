/*
 * pattern2_parent.c — C-only parent for forktest Pattern 2 control experiment
 * (docs/GO_FORKTEST_DEBUG.md): epoll + EPOLLONESHOT, pipe read, epoll_ctl MOD re-arm,
 * children exec /bin/mmap_stress. Mirrors forktest_parent shape without Go.
 *
 * Usage (Akuma): pkg install pattern2_parent mmap_stress [forktest_child]
 *   /bin/pattern2_parent -num_children=1 -duration=10s -mmap_alloc_mb=70
 * C parent + Go child (quadrant vs Go parent + C child — GO_FORKTEST_DEBUG.md):
 *   /bin/pattern2_parent -child=forktest …   # exec /bin/forktest_child -mmap_test=true …
 */

#include <errno.h>
#include <stdint.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef EPOLLONESHOT
#define EPOLLONESHOT (1u << 30)
#endif

static int parse_kv_int(const char *arg, const char *key, int *out) {
    size_t klen = strlen(key);
    if (strncmp(arg, key, klen) != 0) return 0;
    if (arg[klen] != '=') return 0;
    *out = atoi(arg + klen + 1);
    return 1;
}

static long parse_duration_secs(const char *s) {
    char *end = NULL;
    long v = strtol(s, &end, 10);
    if (v <= 0) return 0;
    if (!end || *end == '\0') return v;
    if (end[0] == 's' && end[1] == '\0') return v;
    if (end[0] == 'm' && end[1] == 's' && end[2] == '\0') return v / 1000;
    if (end[0] == 'm' && end[1] == '\0') return v * 60;
    return v;
}

static int parse_kv_duration(const char *arg, const char *key, long *out) {
    size_t klen = strlen(key);
    if (strncmp(arg, key, klen) != 0) return 0;
    if (arg[klen] != '=') return 0;
    long d = parse_duration_secs(arg + klen + 1);
    if (d > 0) *out = d;
    return 1;
}

typedef struct {
    int read_fd;
    pid_t pid;
    int done;
} ChildSlot;

static int parse_kv_child_mode(const char *arg, int *use_forktest_child) {
    const char *keys[] = { "-child=", "--child=", NULL };
    for (int k = 0; keys[k]; k++) {
        size_t len = strlen(keys[k]);
        if (strncmp(arg, keys[k], len) != 0) continue;
        const char *val = arg + len;
        if (strcmp(val, "forktest") == 0) {
            *use_forktest_child = 1;
            return 1;
        }
        if (strcmp(val, "mmap_stress") == 0 || strcmp(val, "c") == 0) {
            *use_forktest_child = 0;
            return 1;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    int num_children = 3;
    long duration_s = 10;
    int mmap_mb = 70;
    int use_forktest_child = 0;

    for (int i = 1; i < argc; i++) {
        int v;
        if (parse_kv_child_mode(argv[i], &use_forktest_child)) {
        } else if (parse_kv_int(argv[i], "-num_children", &v) || parse_kv_int(argv[i], "--num_children", &v)) {
            if (v >= 1) num_children = v;
        } else if (parse_kv_int(argv[i], "-mmap_alloc_mb", &v) ||
                   parse_kv_int(argv[i], "--mmap_alloc_mb", &v)) {
            if (v > 0) mmap_mb = v;
        } else if (parse_kv_duration(argv[i], "-duration", &duration_s) ||
                   parse_kv_duration(argv[i], "--duration", &duration_s)) {
        }
    }

    int epfd = epoll_create1(EPOLL_CLOEXEC);
    if (epfd < 0) {
        perror("epoll_create1");
        return 1;
    }

    ChildSlot *slots = calloc((size_t)num_children, sizeof(ChildSlot));
    if (!slots) {
        fprintf(stderr, "pattern2_parent: calloc failed\n");
        return 1;
    }

    for (int i = 0; i < num_children; i++) {
        int pipefd[2];
        if (pipe(pipefd) < 0) {
            perror("pipe");
            return 1;
        }
        pid_t pid = fork();
        if (pid < 0) {
            perror("fork");
            return 1;
        }
        if (pid == 0) {
            close(pipefd[0]);
            if (dup2(pipefd[1], STDOUT_FILENO) < 0) {
                perror("dup2");
                _exit(126);
            }
            close(pipefd[1]);

            char idbuf[32];
            snprintf(idbuf, sizeof(idbuf), "%d", i);
            setenv("FORKTEST_CHILD_ID", idbuf, 1);

            char arg_mb[64];
            char arg_dur[64];
            snprintf(arg_mb, sizeof(arg_mb), "-mmap_alloc_mb=%d", mmap_mb);
            snprintf(arg_dur, sizeof(arg_dur), "-duration=%lds", (long)duration_s);
            if (use_forktest_child) {
                char mt[] = "-mmap_test=true";
                char *av[] = { "/bin/forktest_child", mt, arg_mb, arg_dur, NULL };
                execv("/bin/forktest_child", av);
                perror("execv /bin/forktest_child");
            } else {
                char *av[] = { "/bin/mmap_stress", arg_dur, arg_mb, NULL };
                execv("/bin/mmap_stress", av);
                perror("execv /bin/mmap_stress");
            }
            _exit(127);
        }
        close(pipefd[1]);
        slots[i].read_fd = pipefd[0];
        slots[i].pid = pid;
        slots[i].done = 0;

        struct epoll_event ev;
        memset(&ev, 0, sizeof(ev));
        ev.events = (uint32_t)(EPOLLIN | EPOLLRDHUP | EPOLLONESHOT);
        ev.data.fd = pipefd[0];
        if (epoll_ctl(epfd, EPOLL_CTL_ADD, pipefd[0], &ev) < 0) {
            perror("epoll_ctl ADD");
            return 1;
        }
    }

    fprintf(stderr,
            "pattern2_parent: %d x %s duration=%lds mb=%d (C parent)\n",
            num_children,
            use_forktest_child ? "forktest_child(Go)" : "mmap_stress(C)",
            duration_s, mmap_mb);

    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    char read_buf[1024];
    int active = num_children;

    while (active > 0) {
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        long elapsed = now.tv_sec - t0.tv_sec;
        if (elapsed >= duration_s) {
            fprintf(stderr, "pattern2_parent: deadline elapsed, SIGTERM children\n");
            for (int i = 0; i < num_children; i++) {
                if (!slots[i].done) {
                    kill(slots[i].pid, SIGTERM);
                }
            }
            break;
        }

        int ms_left = (int)((duration_s - elapsed) * 1000);
        if (ms_left > 100) ms_left = 100;
        if (ms_left < 0) ms_left = 0;

        struct epoll_event events[16];
        int n = epoll_wait(epfd, events, 16, ms_left);
        if (n < 0) {
            if (errno == EINTR) continue;
            perror("epoll_wait");
            break;
        }

        for (int ei = 0; ei < n; ei++) {
            int fd = events[ei].data.fd;
            ChildSlot *ch = NULL;
            for (int j = 0; j < num_children; j++) {
                if (slots[j].read_fd == fd) {
                    ch = &slots[j];
                    break;
                }
            }
            if (!ch) continue;

            if (events[ei].events & EPOLLIN) {
                for (;;) {
                    ssize_t nr = read(fd, read_buf, sizeof(read_buf));
                    if (nr <= 0) break;
                }
            }

            if (events[ei].events & EPOLLRDHUP) {
                for (;;) {
                    ssize_t nr = read(fd, read_buf, sizeof(read_buf));
                    if (nr <= 0) break;
                }
                if (!ch->done) {
                    ch->done = 1;
                    active--;
                }
            }

            if (!ch->done) {
                struct epoll_event rev;
                memset(&rev, 0, sizeof(rev));
                rev.events = (uint32_t)(EPOLLIN | EPOLLRDHUP | EPOLLONESHOT);
                rev.data.fd = fd;
                if (epoll_ctl(epfd, EPOLL_CTL_MOD, fd, &rev) < 0) {
                    perror("epoll_ctl MOD");
                }
            }
        }
    }

    for (int i = 0; i < num_children; i++) {
        waitpid(slots[i].pid, NULL, 0);
        close(slots[i].read_fd);
    }
    close(epfd);
    free(slots);
    fprintf(stderr, "pattern2_parent: done\n");
    return 0;
}
