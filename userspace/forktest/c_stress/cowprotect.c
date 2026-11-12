/*
 * cowprotect.c — does mprotect(PROT_WRITE) after fork() keep copy-on-write?
 *
 * fork() shares every private page read-only; the first write copies it. A child
 * that mprotect()s such a page to PROT_READ and back to PROT_READ|PROT_WRITE must
 * still get its own copy on the next write: the protection changed, the sharing
 * did not. The AArch64 kernel's mprotect granted the write in the page table
 * whenever the region said writable, so the child's store landed in the frame the
 * parent still maps (docs/archive/AKUMA_MPROTECT_CONVERGENCE.md).
 *
 * The MAP_SHARED|MAP_ANONYMOUS leg is the converse: there the same calls must NOT
 * turn the page copy-on-write, or the child's write stops reaching the parent.
 *
 * Static, musl, pure C. Exit 0 = both legs correct.
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o cowprotect cowprotect.c
 */
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int leg(const char *name, int flags, int want_shared) {
    volatile int *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, flags | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 2; }
    *p = 0x1111;
    pid_t c = fork();
    if (c < 0) { perror("fork"); return 2; }
    if (c == 0) {
        if (mprotect((void *)p, 4096, PROT_READ) != 0) _exit(3);
        if (mprotect((void *)p, 4096, PROT_READ | PROT_WRITE) != 0) _exit(4);
        *p = 0x2222;
        _exit(*p == 0x2222 ? 0 : 5);
    }
    int ws = 0;
    if (waitpid(c, &ws, 0) != c || !WIFEXITED(ws) || WEXITSTATUS(ws) != 0) {
        printf("%-12s child failed (status 0x%x)\n", name, ws);
        return 1;
    }
    int v = *p;
    int ok = want_shared ? v == 0x2222 : v == 0x1111;
    printf("%-12s parent sees 0x%x -> %s\n", name, v, ok ? "ok" : "*** WRONG ***");
    munmap((void *)p, 4096);
    return ok ? 0 : 1;
}

int main(void) {
    int r = leg("MAP_PRIVATE", MAP_PRIVATE, 0);
    r |= leg("MAP_SHARED", MAP_SHARED, 1);
    printf("cowprotect: %s\n", r ? "FAIL" : "PASS");
    return r;
}
