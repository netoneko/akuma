// neonstate — is 128-bit NEON (Q) register state preserved across preemption?
//
// The chain of eliminations that led here:
//   1. busybox `md5sum` returns non-deterministic WRONG digests for an
//      unmodified file (reproduced: 10/20 correct, two distinct wrong values).
//   2. `readback verify` on the SAME file: byte-exact, 40/40 iterations. So the
//      bytes are right — ext2 and the page cache are exonerated.
//   3. `computecheck`: an integer (GPR) chain and a `double` (D-register) chain
//      over an in-memory buffer, 400 iterations, 0 wrong. So it is not generic
//      register corruption either.
//
// What (2) and (3) leave is a register class that md5/sha1 and musl's memcpy
// use and neither of (3)'s chains touch: the UPPER 64 bits of the 128-bit V
// registers. AArch64 aliases D0-D31 onto the low halves of Q0-Q31, so a
// save/restore that stores `d0..d31` (64-bit each) rather than `q0..q31`
// preserves a `double` workload perfectly while silently truncating every
// 128-bit vector to its low half.
//
// This probe keeps 16 Q registers live across a long loop and hashes them into
// one value, so any clobber of any lane shows up. Nothing here does I/O; the
// only thing that can perturb it is the kernel taking the core away.
//
// Interpretation:
//   * low_half wrong == 0 AND full wrong > 0  -> upper 64 bits are being lost
//     (that is the save/restore-width bug)
//   * both wrong                              -> whole-register corruption
//   * both clean                              -> NEON is fine; look elsewhere
//
// Calibrated: must report 0/0 on real Linux arm64.
//
// Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o neonstate neonstate.c

#define _GNU_SOURCE
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// Fold 16 live vectors into a 128-bit accumulator, then report both halves
// separately so the width of the damage is visible.
static void vector_chain(const uint8_t *buf, size_t n, uint64_t out[2])
{
    uint32x4_t a0 = vdupq_n_u32(0x11111111), a1 = vdupq_n_u32(0x22222222);
    uint32x4_t a2 = vdupq_n_u32(0x33333333), a3 = vdupq_n_u32(0x44444444);
    uint32x4_t a4 = vdupq_n_u32(0x55555555), a5 = vdupq_n_u32(0x66666666);
    uint32x4_t a6 = vdupq_n_u32(0x77777777), a7 = vdupq_n_u32(0x88888888);
    uint32x4_t b0 = vdupq_n_u32(0x99999999), b1 = vdupq_n_u32(0xAAAAAAAA);
    uint32x4_t b2 = vdupq_n_u32(0xBBBBBBBB), b3 = vdupq_n_u32(0xCCCCCCCC);
    uint32x4_t b4 = vdupq_n_u32(0xDDDDDDDD), b5 = vdupq_n_u32(0xEEEEEEEE);
    uint32x4_t b6 = vdupq_n_u32(0xF0F0F0F0), b7 = vdupq_n_u32(0x0F0F0F0F);

    for (size_t i = 0; i + 16 <= n; i += 16) {
        uint32x4_t v = vreinterpretq_u32_u8(vld1q_u8(buf + i));
        // Every accumulator stays live across the whole loop, and each one's
        // UPPER lanes matter to the final fold.
        a0 = veorq_u32(a0, v);              a1 = vaddq_u32(a1, v);
        a2 = veorq_u32(a2, vshlq_n_u32(v, 1));
        a3 = vaddq_u32(a3, vshrq_n_u32(v, 1));
        a4 = veorq_u32(a4, a0);             a5 = vaddq_u32(a5, a1);
        a6 = veorq_u32(a6, a2);             a7 = vaddq_u32(a7, a3);
        b0 = veorq_u32(b0, a4);             b1 = vaddq_u32(b1, a5);
        b2 = veorq_u32(b2, a6);             b3 = vaddq_u32(b3, a7);
        b4 = veorq_u32(b4, b0);             b5 = vaddq_u32(b5, b1);
        b6 = veorq_u32(b6, b2);             b7 = vaddq_u32(b7, b3);
    }

    uint32x4_t f = veorq_u32(veorq_u32(veorq_u32(a0, a1), veorq_u32(a2, a3)),
                             veorq_u32(veorq_u32(a4, a5), veorq_u32(a6, a7)));
    uint32x4_t g = veorq_u32(veorq_u32(veorq_u32(b0, b1), veorq_u32(b2, b3)),
                             veorq_u32(veorq_u32(b4, b5), veorq_u32(b6, b7)));
    uint32x4_t h = veorq_u32(f, g);
    out[0] = vgetq_lane_u64(vreinterpretq_u64_u32(h), 0);   // low  64 bits (D)
    out[1] = vgetq_lane_u64(vreinterpretq_u64_u32(h), 1);   // high 64 bits (Q only)
}

int main(int argc, char **argv)
{
    size_t len   = (argc > 1) ? (size_t)strtoul(argv[1], NULL, 10) : (256u * 1024u);
    int    iters = (argc > 2) ? atoi(argv[2]) : 400;

    uint8_t *buf = malloc(len);
    if (!buf) { perror("malloc"); return 2; }
    for (size_t i = 0; i < len; i++)
        buf[i] = (uint8_t)((i * 31u + (i >> 5) + 7u) & 0xFF);

    uint64_t want[2];
    vector_chain(buf, len, want);

    int bad_low = 0, bad_high = 0, bad_any = 0;
    uint64_t ex_low = 0, ex_high = 0;

    for (int k = 0; k < iters; k++) {
        uint64_t got[2];
        vector_chain(buf, len, got);
        int lo = (got[0] != want[0]);
        int hi = (got[1] != want[1]);
        if (lo) { if (!bad_low)  ex_low  = got[0]; bad_low++; }
        if (hi) { if (!bad_high) ex_high = got[1]; bad_high++; }
        if (lo || hi) bad_any++;
    }

    printf("neonstate: len=%zu iters=%d\n", len, iters);
    printf("  low 64 bits (aliases D regs) : %d / %d wrong\n", bad_low, iters);
    printf("  high 64 bits (Q only)        : %d / %d wrong\n", bad_high, iters);
    if (bad_low)
        printf("    low  want=%016llx first_wrong=%016llx\n",
               (unsigned long long)want[0], (unsigned long long)ex_low);
    if (bad_high)
        printf("    high want=%016llx first_wrong=%016llx\n",
               (unsigned long long)want[1], (unsigned long long)ex_high);
    if (bad_high && !bad_low)
        printf("  VERDICT: upper 64 bits of V registers are NOT preserved "
               "(save/restore width bug)\n");
    else if (bad_any)
        printf("  VERDICT: vector state corrupted in both halves\n");
    else
        printf("  VERDICT: NEON state is preserved\n");
    printf("RESULT: %s\n", bad_any == 0 ? "PASS" : "FAIL");
    return bad_any == 0 ? 0 : 1;
}
