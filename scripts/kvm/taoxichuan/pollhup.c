/* pollhup - the loop std::process::Command::output() runs (library/std/src/sys/pal/unix/pipe.rs read2):
 * spawn a child with stdout and stderr on pipes, poll both read ends, read what is ready, stop at EOF on both.
 * Prints each poll's return and revents, so a kernel that answers "ready" forever (or with bits the loop does
 * not consume) shows as a spin. Expect: a few iterations, both pipes reach EOF, child reaped, rc 0. */
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
int main(int argc, char **argv) {
    int o[2], e[2];
    if (pipe(o) || pipe(e)) { perror("pipe"); return 2; }
    pid_t p = fork();
    if (p == 0) {
        dup2(o[1], 1); dup2(e[1], 2);
        close(o[0]); close(o[1]); close(e[0]); close(e[1]);
        execl("/bin/busybox", "busybox", "echo", "hello-from-child", (char *)0);
        _exit(127);
    }
    close(o[1]); close(e[1]);
    /* Rust's read2 and cargo's put both pipes in O_NONBLOCK before polling. A non-blocking read of a pipe whose
     * writers are gone must be 0 (EOF), never EAGAIN: with EAGAIN the loop polls (EOF is readable), reads, and
     * retries for ever - the cargo stall of 2026-10-31 (docs/archive/TAOXICHUAN_CARGO_STALL_REGRESSION.md). */
    fcntl(o[0], F_SETFL, fcntl(o[0], F_GETFL) | O_NONBLOCK);
    fcntl(e[0], F_SETFL, fcntl(e[0], F_GETFL) | O_NONBLOCK);
    struct pollfd fds[2] = {{o[0], POLLIN, 0}, {e[0], POLLIN, 0}};
    int open_[2] = {1, 1};
    for (int it = 0; it < 40 && (open_[0] || open_[1]); it++) {
        int n = poll(fds, 2, 5000);
        printf("it=%d poll=%d rev0=0x%x rev1=0x%x\n", it, n, fds[0].revents, fds[1].revents);
        for (int i = 0; i < 2; i++) {
            if (!open_[i] || fds[i].revents == 0) continue;
            char b[256];
            ssize_t r = read(fds[i].fd, b, sizeof b);
            printf("  fd%d read=%zd errno=%d\n", i, r, r < 0 ? errno : 0);
            if (r < 0 && errno == EAGAIN && (fds[i].revents & (POLLHUP | POLLIN)) && it > 3) { /* readable yet "not yet" */ }
            if (r == 0) { open_[i] = 0; fds[i].fd = -1; }
        }
    }
    int st = 0;
    pid_t w = waitpid(p, &st, 0);
    printf("waitpid=%d status=0x%x open=%d,%d %s\n", (int)w, st, open_[0], open_[1], (open_[0] || open_[1]) ? "SPIN-BUG" : "OK");
    return (open_[0] || open_[1]) ? 1 : 0;
}
