/*
 * neonfault.c — data integrity of NEON loads that CROSS a page boundary into
 * an unmapped (demand-paged) page of a file-backed mmap.
 *
 * llama.cpp's quantized GEMM reads weights with unaligned NEON loads (Q4
 * blocks are 18-byte strided), so on an mmap'd model the load itself faults —
 * with its value spanning [resident page | faulting page]. This probes exactly
 * that shape: for every page boundary in the mapping, `ld1 {v0.16b}` at
 * (boundary - 8) and compare the 16 bytes against a pread() reference.
 * Boundaries that sit at the edge of a readahead batch genuinely fault
 * mid-instruction; the rest are resident controls.
 *
 * Any mismatch = the kernel's fault path resumed a partially-executed or
 * mis-fixed-up NEON load — numeric corruption invisible to integer probes.
 *
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o neonfault neonfault.c
 * Usage: neonfault <path>
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define PAGE 4096UL

static void neon_load16(const volatile unsigned char *p, unsigned char out[16]) {
    asm volatile(
        "ld1 {v0.16b}, [%[src]]\n"
        "st1 {v0.16b}, [%[dst]]\n"
        :
        : [src] "r"(p), [dst] "r"(out)
        : "v0", "memory");
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: neonfault <path>\n");
        return 2;
    }
    int fd = open(argv[1], O_RDONLY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_size < (off_t)(2 * PAGE)) {
        fprintf(stderr, "neonfault: open/fstat(%s) failed\n", argv[1]);
        return 2;
    }
    size_t size = (size_t)st.st_size & ~(PAGE - 1);

    const volatile unsigned char *m =
        mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if ((const void *)m == MAP_FAILED) {
        fprintf(stderr, "neonfault: mmap failed\n");
        return 2;
    }

    unsigned long boundaries = size / PAGE - 1;
    unsigned long bad = 0;
    unsigned char got[16], want[16];
    printf("neonfault: %lu page-crossing NEON loads\n", boundaries);
    fflush(stdout);

    for (unsigned long i = 1; i <= boundaries; i++) {
        size_t off = i * PAGE - 8; /* 8 bytes before the boundary, 8 after */
        neon_load16(m + off, got);
        if (pread(fd, want, 16, (off_t)off) != 16) {
            fprintf(stderr, "neonfault: pread failed at %zu\n", off);
            return 2;
        }
        if (memcmp(got, want, 16) != 0) {
            bad++;
            if (bad <= 8) {
                printf("neonfault: MISMATCH at boundary %lu (off %zu):\n  got  ", i, off);
                for (int b = 0; b < 16; b++) printf("%02x", got[b]);
                printf("\n  want ");
                for (int b = 0; b < 16; b++) printf("%02x", want[b]);
                printf("\n");
                fflush(stdout);
            }
        }
    }
    printf("neonfault: done, %lu/%lu crossing loads wrong\n", bad, boundaries);
    return bad ? 1 : 0;
}
