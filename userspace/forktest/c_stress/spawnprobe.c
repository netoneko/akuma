/*
 * spawnprobe: musl posix_spawn (clone(CLONE_VM|CLONE_VFORK) on the caller's
 * stack, then execve; errno comes back through a CLOEXEC pipe) — the way
 * crashpad starts chrome_crashpad_handler, which fails ENOENT on Akuma.
 *   spawnprobe [path [arg...]]   default: /bin/busybox true
 * Each case prints posix_spawn's return and the child's wait status. Linux:
 * every existing path returns 0 and exits 0; the missing one returns 2.
 */
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
static int fails;

static void one(const char *label, char *const argv[], int want_rc) {
    pid_t pid = -1;
    int rc = posix_spawn(&pid, argv[0], NULL, NULL, argv, environ);
    int st = -1;
    if (rc == 0) waitpid(pid, &st, 0);
    int ok = rc == want_rc && (rc != 0 || (WIFEXITED(st) && WEXITSTATUS(st) == 0));
    printf("spawnprobe[%s] posix_spawn(%s) = %d (%s) exit=%d %s\n", label, argv[0], rc,
           rc ? strerror(rc) : "ok", rc == 0 && WIFEXITED(st) ? WEXITSTATUS(st) : -1,
           ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

int main(int argc, char **argv) {
    char *dflt[] = {"/bin/busybox", "true", NULL};
    char **target = argc > 1 ? argv + 1 : dflt;
    char *missing[] = {"/no/such/binary", NULL};

    one("direct", target, 0);
    one("missing", missing, ENOENT);

    /* crashpad's shape: fork, setsid, close inherited fds, then posix_spawn
     * from the child and report through the exit status. */
    pid_t p = fork();
    if (p == 0) {
        setsid();
        for (int fd = 3; fd < 64; fd++) close(fd);
        fails = 0;
        one("fork+setsid", target, 0);
        _exit(fails);
    }
    int st = 0;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st)) fails++;

    /* A thread-free vfork + execve, without posix_spawn's pipe. */
    pid_t v = vfork();
    if (v == 0) {
        execv(target[0], target);
        _exit(100 + errno);
    }
    waitpid(v, &st, 0);
    printf("spawnprobe[vfork] execv(%s) exit=%d %s\n", target[0],
           WIFEXITED(st) ? WEXITSTATUS(st) : -1, WIFEXITED(st) && !WEXITSTATUS(st) ? "ok" : "FAIL");
    if (!WIFEXITED(st) || WEXITSTATUS(st)) fails++;

    printf("spawnprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
