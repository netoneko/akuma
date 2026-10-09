/* mapnoreplace.c — does MAP_FIXED_NOREPLACE mean what Linux (>= 4.17) says?
 *
 * Why: on Akuma/amd64 (2026-10-09) a MAP_FIXED_NOREPLACE request for a free
 * address came back somewhere else (`altstackexec.c` found it): the flag fell
 * through to the "hint" path. Allocators that reserve fixed ranges this way
 * (PartitionAlloc, V8's code range, glibc's own pool code) then use memory
 * they did not get.
 *
 * Linux semantics checked (mmap(2)):
 *   free address            mapped exactly there
 *   same address again      fails with EEXIST, the first mapping untouched
 *   over the program text   EEXIST (no region record needed to count)
 *   over the stack          EEXIST
 *
 * Build: x86_64-linux-musl-gcc -O2 -static -o mapnoreplace mapnoreplace.c
 * Run:   ./mapnoreplace       (PASS/FAIL per case, exit 0 iff all pass)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

static int fails;

static void check(const char *what, int ok, const char *detail) {
    printf("mapnoreplace: %-26s %s %s\n", what, ok ? "PASS" : "FAIL", detail);
    fails |= !ok;
}

static void *nr(void *at, size_t len) {
    return mmap(at, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
}

int main(void) {
    char buf[96];
    void *want = (void *)0x5b5b00000000ULL;
    void *p = nr(want, 65536);
    snprintf(buf, sizeof buf, "(got %p)", p);
    check("free address", p == want, buf);
    if (p == want) *(volatile int *)p = 42;

    errno = 0;
    void *q = nr(want, 4096);
    snprintf(buf, sizeof buf, "(got %p errno %d)", q, errno);
    check("same address again", q == MAP_FAILED && errno == EEXIST, buf);
    if (p == want) check("first mapping intact", *(volatile int *)p == 42, "");

    errno = 0;
    void *text = (void *)((uintptr_t)&main & ~(uintptr_t)4095);
    void *t = nr(text, 4096);
    snprintf(buf, sizeof buf, "(text %p got %p errno %d)", text, t, errno);
    check("over program text", t == MAP_FAILED && errno == EEXIST, buf);

    errno = 0;
    int local;
    void *stk = (void *)((uintptr_t)&local & ~(uintptr_t)4095);
    void *s = nr(stk, 4096);
    snprintf(buf, sizeof buf, "(stack %p got %p errno %d)", stk, s, errno);
    check("over the stack", s == MAP_FAILED && errno == EEXIST, buf);

    printf("mapnoreplace: %s\n", fails ? "FAIL" : "all PASS");
    return fails;
}
