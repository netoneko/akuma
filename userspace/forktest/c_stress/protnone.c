/*
 * protnone - PROT_NONE is enforced, for user accesses and for the kernel's copies.
 *
 * Each access that must fault runs in a fork child, so the probe needs no
 * SIGSEGV handler (a kernel that only delivers signals at syscall return still
 * gets a verdict): the child touching the page must die of SIGSEGV. Covers a
 * page never touched (mapped PROT_NONE, so it has no frame yet) and a present
 * page turned PROT_NONE by mprotect, whose contents must come back when it is
 * made readable again. read(2) into and write(2) from a PROT_NONE buffer must
 * be EFAULT, not a silent copy. docs/archive/TAOXICHUAN_LAZY_MMAP.md
 *
 *   x86_64-linux-musl-gcc -static -O2 -Wall -Wextra -o protnone protnone.c
 */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;

static void report(const char *name, int ok)
{
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
    if (!ok)
        fails++;
}

/* Does reading (write=0) or writing (write=1) `p` kill a child with SIGSEGV? */
static int faults(volatile char *p, int write)
{
    pid_t c = fork();
    if (c == 0) {
        if (write)
            *p = 1;
        else
            (void)*p;
        _exit(0);
    }
    int ws = 0;
    waitpid(c, &ws, 0);
    return WIFSIGNALED(ws) && WTERMSIG(ws) == SIGSEGV;
}

int main(void)
{
    /* Never touched: no frame behind it. */
    char *n = mmap(NULL, 2 * 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    report("mmap PROT_NONE", n != MAP_FAILED);
    report("read of an untouched PROT_NONE page faults", faults(n, 0));
    report("write of an untouched PROT_NONE page faults", faults(n + 4096, 1));
    report("mprotect it RW", mprotect(n, 2 * 4096, PROT_READ | PROT_WRITE) == 0);
    n[0] = 'a';
    report("then it is usable and zero-filled", n[0] == 'a' && n[4096] == 0);

    /* Present, with contents, then PROT_NONE. */
    char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(p, 'x', 4096);
    report("mprotect a written page PROT_NONE", mprotect(p, 4096, PROT_NONE) == 0);
    report("read of it faults", faults(p, 0));
    report("write of it faults", faults(p, 1));

    int fd = open("/proc/self/exe", O_RDONLY);
    errno = 0;
    report("read(2) into it is EFAULT", read(fd, p, 16) == -1 && errno == EFAULT);
    close(fd);
    int pf[2];
    pipe(pf);
    errno = 0;
    report("write(2) from it is EFAULT", write(pf[1], p, 16) == -1 && errno == EFAULT);

    report("mprotect it back to read", mprotect(p, 4096, PROT_READ) == 0);
    report("the contents survived", p[0] == 'x' && p[4095] == 'x');
    report("a read-only page still refuses writes", faults(p, 1));

    printf("%s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails != 0;
}
