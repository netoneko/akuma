/*
 * snapprobe: how a zygote loads v8_context_snapshot.bin on Linux (strace):
 * readlink(/proc/self/exe), open, fstat, mmap(NULL, size, PROT_READ,
 * MAP_SHARED, fd, 0) with size not a page multiple. Compares the mapping with
 * pread() byte for byte, then repeats in a fork+execve'd child — the zygote is
 * an exec of the browser. Linux: identical both times.
 *   snapprobe [file]   default /usr/lib/chromium/v8_context_snapshot.bin
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int check(const char *who, const char *path) {
    char exe[256] = {0};
    readlink("/proc/self/exe", exe, sizeof exe - 1);
    int fd = open(path, O_RDONLY);
    struct stat sb;
    if (fd < 0 || fstat(fd, &sb) < 0) { printf("snapprobe[%s] open/fstat %s: FAIL\n", who, path); return 1; }
    unsigned char *m = mmap(NULL, sb.st_size, PROT_READ, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) { printf("snapprobe[%s] mmap: FAIL\n", who); return 1; }
    unsigned char *b = malloc(sb.st_size);
    ssize_t got = pread(fd, b, sb.st_size, 0);
    long bad = -1;
    for (long i = 0; got == sb.st_size && i < sb.st_size; i++)
        if (m[i] != b[i]) { bad = i; break; }
    int ok = got == sb.st_size && bad < 0;
    printf("snapprobe[%s] exe=%s size=%lld pread=%zd first-diff=%ld head=%02x%02x%02x%02x tail=%02x %s\n",
           who, exe, (long long)sb.st_size, got, bad, m[0], m[1], m[2], m[3], m[sb.st_size - 1],
           ok ? "ok" : "FAIL");
    fflush(stdout);
    return !ok;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 && strcmp(argv[1], "--child") ? argv[1] : "/usr/lib/chromium/v8_context_snapshot.bin";
    if (argc > 2 && !strcmp(argv[2], "--child")) return check("child", path);
    int fails = check("parent", path);
    pid_t p = fork();
    if (p == 0) {
        char *av[] = {"snapprobe", (char *)path, "--child", NULL};
        execv("/proc/self/exe", av);
        _exit(99);
    }
    int st = 0;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st)) fails++;
    printf("snapprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
