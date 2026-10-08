/*
 * jitprobe: V8's code range as Chromium's renderer sets it up (seen in the
 * Linux strace: mmap 512 MB PROT_NONE, prctl(PR_SET_VMA_ANON_NAME "v8"),
 * mprotect the whole range PROT_READ|PROT_WRITE|PROT_EXEC, madvise
 * MADV_DONTNEED), then what a JIT does with it: write machine code into a
 * page and call it. Also mmap(PROT_RWX) directly, and an mprotect of a
 * sub-range from RWX to RX, since V8 flips pages that way too.
 *
 * Akuma amd64 before 2026-10-08 refused PROT_WRITE|PROT_EXEC in both mmap and
 * mprotect with EINVAL (a W^X rule Linux does not have); every renderer then
 * died on a CHECK (`[Fault] #BP`) right after the mprotect, and the browser
 * sat waiting for one. Linux: everything ok, the called code returns 42.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <unistd.h>

#ifndef PR_SET_VMA
#define PR_SET_VMA 0x53564d41
#define PR_SET_VMA_ANON_NAME 0
#endif

static int fails;

static void check(const char *what, int ok) {
    printf("jitprobe: %s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}

/* x86_64: mov eax, 42; ret */
static const unsigned char code[] = { 0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3 };

static int call_code(void *p) {
    memcpy(p, code, sizeof code);
    __builtin___clear_cache((char *)p, (char *)p + sizeof code);
    int (*fn)(void) = (int (*)(void))p;
    return fn();
}

int main(void) {
    const size_t range = 512u << 20;
    void *r = mmap(NULL, range, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("mmap 512 MB PROT_NONE", r != MAP_FAILED);
    if (r == MAP_FAILED) { printf("jitprobe: FAIL\n"); return 1; }
    /* Linux names it; a kernel without PR_SET_VMA answers EINVAL and V8
     * ignores that, so it is reported, not scored. */
    int named = prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, (unsigned long)r, range, "v8");
    printf("jitprobe: prctl(PR_SET_VMA_ANON_NAME) = %d%s\n", named, named ? " (errno set; not scored)" : "");

    int rc = mprotect(r, range, PROT_READ | PROT_WRITE | PROT_EXEC);
    if (rc != 0) perror("jitprobe: mprotect RWX");
    check("mprotect whole range RWX == 0", rc == 0);
    check("madvise(MADV_DONTNEED) whole range == 0", madvise(r, range, MADV_DONTNEED) == 0);
    if (rc == 0) {
        /* A page in the middle, as V8 would allocate. */
        char *page = (char *)r + (64u << 20);
        check("code written into the RWX range runs (42)", call_code(page) == 42);
        /* V8's write-protect flip: a sub-range back to RX, then the code still runs. */
        check("mprotect sub-range RX == 0", mprotect(page, 4096, PROT_READ | PROT_EXEC) == 0);
        int (*fn)(void) = (int (*)(void))page;
        check("code still runs after RX (42)", fn() == 42);
    }
    munmap(r, range);

    void *d = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (d == MAP_FAILED) perror("jitprobe: mmap RWX");
    check("mmap(PROT_RWX) directly", d != MAP_FAILED);
    if (d != MAP_FAILED) {
        check("code in a direct RWX page runs (42)", call_code(d) == 42);
        munmap(d, 4096);
    }
    printf("jitprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
