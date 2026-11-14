// mapfixed — MAP_FIXED over a live anonymous mapping must replace it.
//
// POSIX: a MAP_FIXED mapping discards whatever was mapped in its range. The
// replaced pages must read as zero, the old mapping must be gone from
// /proc/self/maps as a separate entry, and repeating the cycle must not leak:
// each round maps, overwrites with MAP_FIXED, then unmaps, so free memory after
// the rounds should match before.
//
//   mapfixed [rounds=2000] [pages=4]
//
// Prints one line per check and PASS/FAIL. Linux is the oracle.

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysinfo.h>
#include <unistd.h>

static long free_kib(void) {
    struct sysinfo si;
    if (sysinfo(&si) != 0) return -1;
    return (long)(si.freeram * si.mem_unit / 1024);
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 2000;
    size_t pages = argc > 2 ? (size_t)atoi(argv[2]) : 4;
    size_t len = pages * 4096;
    int fail = 0;

    // 1. The replaced range reads as zero.
    unsigned char *a = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (a == MAP_FAILED) { perror("mmap"); return 1; }
    memset(a, 0xAB, len);
    unsigned char *b = mmap(a, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    int zero = b == a;
    for (size_t i = 0; zero && i < len; i++) zero = b[i] == 0;
    printf("fixed_replaces_with_zero %s\n", zero ? "ok" : "FAIL");
    fail |= !zero;

    // 2. munmap of the replacement leaves nothing mapped there.
    munmap(a, len);
    FILE *m = fopen("/proc/self/maps", "r");
    char line[256];
    int stale = 0;
    unsigned long want = (unsigned long)a;
    while (m && fgets(line, sizeof line, m)) {
        unsigned long lo, hi;
        if (sscanf(line, "%lx-%lx", &lo, &hi) == 2 && lo <= want && want < hi) stale = 1;
    }
    if (m) fclose(m);
    printf("unmapped_after_replace %s\n", stale ? "FAIL (still listed)" : "ok");
    fail |= stale;

    // 3. Repeat the replace cycle; free memory must come back.
    long before = free_kib();
    for (int r = 0; r < rounds; r++) {
        unsigned char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { printf("round %d: mmap failed\n", r); fail = 1; break; }
        memset(p, r, len);
        unsigned char *q = mmap(p, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        if (q != p) { printf("round %d: MAP_FIXED moved\n", r); fail = 1; break; }
        q[0] = 1;
        munmap(q, len);
    }
    long after = free_kib();
    long lost = before - after;
    // Allow 1 MiB of noise for unrelated allocations.
    int leak = lost > 1024;
    printf("replace_cycle_rounds=%d free_before=%ldK free_after=%ldK lost=%ldK %s\n",
           rounds, before, after, lost, leak ? "FAIL" : "ok");
    fail |= leak;

    printf("mapfixed: %s\n", fail ? "FAIL" : "PASS");
    return fail;
}
