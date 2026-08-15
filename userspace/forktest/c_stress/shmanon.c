/*
 * shmanon.c — is MAP_SHARED|MAP_ANONYMOUS actually shared across fork()?
 *
 * Linux semantics: a MAP_SHARED anonymous mapping survives fork as ONE object, so
 * the parent sees the child's write. MAP_PRIVATE is copy-on-write and does not.
 * This checks both directions in one run, because only testing MAP_SHARED cannot
 * tell "sharing is broken" from "the child never ran".
 *
 * **Akuma FAILS the MAP_SHARED leg (2026-08-15):** the parent reads back its own
 * value, i.e. the mapping behaves exactly like MAP_PRIVATE — fork copies it instead
 * of sharing it. Found because `fpcpoison`'s cross-process start gate silently never
 * released: its children incremented a counter in a MAP_SHARED page that the parent
 * could not see, so every "concurrent" round actually ran unsynchronised. Any probe
 * that coordinates processes through MAP_SHARED anonymous memory is measuring
 * something other than what it claims on this kernel.
 *
 * Calibrated ALL PASS on real Linux arm64 and on macOS arm64; a FAIL here is the
 * kernel. See docs/archive/SELFHOST_ZERO_PAGE_HUNT.md.
 *
 * Static, musl, pure C.
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o shmanon shmanon.c
 * Usage: shmanon          (exit 0 = both legs correct)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    volatile int *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) { perror("mmap MAP_SHARED"); return 2; }
    *shared = 0;

    volatile int *priv = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (priv == MAP_FAILED) { perror("mmap MAP_PRIVATE"); return 2; }
    *priv = 0;

    pid_t p = fork();
    if (p < 0) { perror("fork"); return 2; }
    if (p == 0) { *shared = 0x5eed; *priv = 0x5eed; _exit(0); }
    int ws = 0; wait(&ws);

    int s = *shared, v = *priv;
    printf("after child wrote 0x5eed:\n");
    printf("  MAP_SHARED  parent sees 0x%x  -> %s\n", s,
           s == 0x5eed ? "SHARED (correct)" : "*** NOT SHARED — behaves like MAP_PRIVATE ***");
    printf("  MAP_PRIVATE parent sees 0x%x  -> %s\n", v,
           v == 0 ? "isolated (correct)" : "*** LEAKED across fork ***");
    return (s == 0x5eed && v == 0) ? 0 : 1;
}
