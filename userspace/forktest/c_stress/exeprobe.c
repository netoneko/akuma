/*
 * exeprobe: what readlink("/proc/self/exe") says before and after a re-exec
 * through execve("/proc/self/exe") — the way Chromium starts every child and
 * then finds its own files beside the answer — and after the process renames
 * itself with prctl(PR_SET_NAME), which Chromium's browser does too.
 * Linux: every line names the binary's path; the rename changes only `comm`.
 * Akuma before 2026-10-08 answered the new comm ("renamed") after the rename,
 * which sent Chromium looking for its files in "".
 */
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

static ssize_t exe(char *b, size_t n) {
    memset(b, 0, n);
    return readlink("/proc/self/exe", b, n - 1);
}

int main(int argc, char **argv) {
    char b[256], c[256];
    ssize_t n = exe(b, sizeof b);
    printf("exeprobe[%s] readlink(/proc/self/exe) = %zd '%s'\n", argc > 1 ? argv[1] : "parent", n, b);
    prctl(PR_SET_NAME, "renamed", 0, 0, 0);
    ssize_t m = exe(c, sizeof c);
    int same = m == n && !strcmp(b, c);
    printf("exeprobe[%s] after PR_SET_NAME: '%s' %s\n", argc > 1 ? argv[1] : "parent", c,
           same ? "ok" : "FAIL");
    fflush(stdout);
    if (argc == 1) {
        char *av[] = {"exeprobe", "child-via-proc-self-exe", NULL};
        execv("/proc/self/exe", av);
        perror("execv /proc/self/exe");
    }
    return !same;
}
