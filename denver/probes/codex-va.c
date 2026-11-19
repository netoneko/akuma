/* The optional Denver Codex profile bounds per-call page-table work.
 * Run only on a kernel built with --features codex. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/mman.h>

static int failures;
static void check(const char *name, int ok) {
    printf("codex-va: %s %s\n", name, ok ? "PASS" : "FAIL");
    failures += !ok;
}
int main(void) {
    const size_t len = ((size_t)16 << 30) + 4096;
    void *addr = (void *)0x600000000000ULL;
    errno = 0;
    void *p = mmap(NULL, len, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("oversized mmap refused", p == MAP_FAILED && errno == ENOMEM);
    if (p != MAP_FAILED) munmap(p, len);
    errno = 0;
    check("oversized mprotect refused", mprotect(addr, len, PROT_READ) == -1 && errno == ENOMEM);
    errno = 0;
    check("oversized munmap refused", munmap(addr, len) == -1 && errno == EINVAL);
    errno = 0;
    check("oversized madvise refused", madvise(addr, len, MADV_DONTNEED) == -1 && errno == EINVAL);
    errno = 0;
    check("oversized mremap refused", mremap(addr, 4096, len, MREMAP_MAYMOVE) == MAP_FAILED && errno == ENOMEM);
    printf("codex-va: %s\n", failures ? "FAIL" : "PASS");
    return failures != 0;
}
