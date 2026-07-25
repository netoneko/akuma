/*
 * fpfault.c — FP/NEON register integrity across demand-paging faults.
 *
 * Hypothesis probe for the "llama garbage with mmap, clean with --no-mmap"
 * corruption: an mmap'd model makes NEON-heavy compute take INVOLUNTARY
 * demand-paging faults mid-GEMM, where all 32 Q registers are live. If the
 * EL0 data-abort path (or the kernel fill code it runs) clobbers any Q
 * register, inference numerics corrupt silently. Integer hashing (mmapsum)
 * cannot see this — only FP state does.
 *
 * Method: for every page of a file-backed mapping, load all 32 Q registers
 * with a per-iteration pattern, touch the (unmapped, will-fault) page with an
 * integer load, store the Q registers back out, and compare. Any mismatch is
 * reported with the register index and iteration. Exit 0 = no corruption.
 *
 * Static, musl, pure C + inline asm — kernel-fault attribution.
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o fpfault fpfault.c
 * Usage: fpfault <path>   (bigger file = more faulted pages = more trials)
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define PAGE 4096UL

/* 32 Q regs * 16 bytes. */
static unsigned char pat_in[512] __attribute__((aligned(16)));
static unsigned char pat_out[512] __attribute__((aligned(16)));

/* Load v0..v31 from pat_in, do a faulting integer load from `p`, store
 * v0..v31 to pat_out. The faulting load uses only x registers, so any
 * difference between pat_in and pat_out was introduced by the kernel's
 * fault handling. Everything in ONE asm block so the compiler can't
 * reschedule FP code between the loads and the fault. */
static unsigned long touch_with_fp_canary(const volatile unsigned char *p) {
    unsigned long sink;
    register const unsigned char *in asm("x9") = pat_in;
    register unsigned char *out asm("x10") = pat_out;
    asm volatile(
        "ld1 {v0.16b-v3.16b},   [%[in]], #64\n"
        "ld1 {v4.16b-v7.16b},   [%[in]], #64\n"
        "ld1 {v8.16b-v11.16b},  [%[in]], #64\n"
        "ld1 {v12.16b-v15.16b}, [%[in]], #64\n"
        "ld1 {v16.16b-v19.16b}, [%[in]], #64\n"
        "ld1 {v20.16b-v23.16b}, [%[in]], #64\n"
        "ld1 {v24.16b-v27.16b}, [%[in]], #64\n"
        "ld1 {v28.16b-v31.16b}, [%[in]], #64\n"
        "ldr %[sink], [%[page]]\n" /* <-- demand fault happens here */
        "st1 {v0.16b-v3.16b},   [%[out]], #64\n"
        "st1 {v4.16b-v7.16b},   [%[out]], #64\n"
        "st1 {v8.16b-v11.16b},  [%[out]], #64\n"
        "st1 {v12.16b-v15.16b}, [%[out]], #64\n"
        "st1 {v16.16b-v19.16b}, [%[out]], #64\n"
        "st1 {v20.16b-v23.16b}, [%[out]], #64\n"
        "st1 {v24.16b-v27.16b}, [%[out]], #64\n"
        "st1 {v28.16b-v31.16b}, [%[out]], #64\n"
        : [sink] "=&r"(sink), [in] "+r"(in), [out] "+r"(out)
        : [page] "r"(p)
        : "v0","v1","v2","v3","v4","v5","v6","v7",
          "v8","v9","v10","v11","v12","v13","v14","v15",
          "v16","v17","v18","v19","v20","v21","v22","v23",
          "v24","v25","v26","v27","v28","v29","v30","v31",
          "memory");
    return sink;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: fpfault <path>\n");
        return 2;
    }
    int fd = open(argv[1], O_RDONLY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_size < (off_t)PAGE) {
        fprintf(stderr, "fpfault: open/fstat(%s) failed\n", argv[1]);
        return 2;
    }
    size_t size = (size_t)st.st_size & ~(PAGE - 1);

    const volatile unsigned char *m =
        mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if ((const void *)m == MAP_FAILED) {
        fprintf(stderr, "fpfault: mmap failed\n");
        return 2;
    }

    unsigned long pages = size / PAGE;
    unsigned long bad = 0;
    volatile unsigned long sink = 0;
    printf("fpfault: %lu pages, canary in all 32 Q regs across each fault\n", pages);
    fflush(stdout);

    for (unsigned long i = 0; i < pages; i++) {
        /* Fresh pattern each iteration so a stale save/restore also trips. */
        for (int b = 0; b < 512; b++)
            pat_in[b] = (unsigned char)(b * 31 + i * 131 + 7);
        memset(pat_out, 0, sizeof(pat_out));

        sink += touch_with_fp_canary(m + i * PAGE);

        if (memcmp(pat_in, pat_out, 512) != 0) {
            bad++;
            if (bad <= 8) {
                int reg = -1;
                for (int b = 0; b < 512; b++)
                    if (pat_in[b] != pat_out[b]) { reg = b / 16; break; }
                printf("fpfault: CORRUPTED page %lu first-bad-reg v%d\n", i, reg);
                fflush(stdout);
            }
        }
    }
    printf("fpfault: done, %lu/%lu faults corrupted FP state, sink=%lu\n",
           bad, pages, (unsigned long)sink);
    return bad ? 1 : 0;
}
