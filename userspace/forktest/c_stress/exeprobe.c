/*
 * exeprobe: what readlink("/proc/self/exe") says before and after a re-exec
 * through execve("/proc/self/exe") — the way Chromium starts every child and
 * then finds its own files beside the answer. Both lines must name the real
 * binary (Linux: both print the binary's path).
 */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    char b[256] = {0};
    ssize_t n = readlink("/proc/self/exe", b, sizeof b - 1);
    printf("exeprobe[%s] readlink(/proc/self/exe) = %zd '%s'\n", argc > 1 ? argv[1] : "parent", n, b);
    fflush(stdout);
    if (argc == 1) {
        char *av[] = {"exeprobe", "child-via-proc-self-exe", NULL};
        execv("/proc/self/exe", av);
        perror("execv /proc/self/exe");
    }
    return 0;
}
