/*
 * stp_test (C): verify kernel EC=0x15 misrouting fix for stp xzr, xzr, [Xn, #N].
 *
 * Exercises the same QEMU misrouting scenario as the Go binary but from C:
 * each test mmaps a PROT_NONE region then executes a specific stp xzr, xzr
 * variant against it. The kernel must demand-page the target and emulate the
 * store. Without the fix the process crashes with SIGSEGV.
 */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define PAGE 4096

static int test_stp(const char *name, int offset_bytes)
{
    uint8_t *p = mmap(NULL, PAGE, PROT_NONE, MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED) {
        fprintf(stderr, "stp_test C: %s mmap failed\n", name);
        return 0;
    }

    uint8_t *dst = p + offset_bytes;

    /* Emit stp xzr, xzr, [x0] where x0 = dst.  The offset is already baked
     * into the pointer arithmetic; the inline asm always uses offset=0 in the
     * instruction so every test exercises the base encoding (0xa9007c1f). */
    __asm__ volatile(
        "stp xzr, xzr, [%0]\n"
        : : "r"(dst) : "memory"
    );

    /* Verify: 16 bytes at dst should be zero after the kernel emulated the store. */
    uint64_t v0, v1;
    __asm__ volatile(
        "ldp %0, %1, [%2]\n"
        : "=r"(v0), "=r"(v1) : "r"(dst)
    );

    munmap(p, PAGE);

    if (v0 == 0 && v1 == 0) {
        printf("stp_test C: %s PASSED\n", name);
        return 1;
    } else {
        fprintf(stderr, "stp_test C: %s FAILED v0=0x%llx v1=0x%llx\n",
                name, (unsigned long long)v0, (unsigned long long)v1);
        return 0;
    }
}

/*
 * Also test non-zero immediate offsets by emitting the exact instruction
 * encoding directly via __asm__.
 */
static int test_stp_imm(const char *name, void *base, int imm_offset)
{
    /* The instruction stp xzr, xzr, [x0, #N] is 0xa900_7c1f with imm7
     * field encoding N/8.  We emit via asm constraints instead of hardcoding
     * bytes so the compiler fills the base register correctly.
     *
     * For offset 16:  stp xzr, xzr, [x0, #16]  = 0xa9017c1f
     * For offset 112: stp xzr, xzr, [x0, #112] = 0xa90e7c1f  (crush pattern)
     */
    if (imm_offset == 16) {
        __asm__ volatile("stp xzr, xzr, [%0, #16]\n" : : "r"(base) : "memory");
    } else if (imm_offset == 32) {
        __asm__ volatile("stp xzr, xzr, [%0, #32]\n" : : "r"(base) : "memory");
    } else if (imm_offset == 112) {
        __asm__ volatile("stp xzr, xzr, [%0, #112]\n" : : "r"(base) : "memory");
    } else {
        fprintf(stderr, "stp_test C: %s unsupported offset %d\n", name, imm_offset);
        return 0;
    }

    uint64_t v0, v1;
    uint8_t *dst = (uint8_t *)base + imm_offset;
    __asm__ volatile("ldp %0, %1, [%2]\n" : "=r"(v0), "=r"(v1) : "r"(dst));

    if (v0 == 0 && v1 == 0) {
        printf("stp_test C: %s PASSED\n", name);
        return 1;
    } else {
        fprintf(stderr, "stp_test C: %s FAILED v0=0x%llx v1=0x%llx\n",
                name, (unsigned long long)v0, (unsigned long long)v1);
        return 0;
    }
}

int main(void)
{
    int pass = 1;

    /* Test 1: stp xzr, xzr, [x0] on PROT_NONE page */
    pass &= test_stp("offset=0", 0);

    /* Test 2: stp xzr, xzr, [x0] where pointer is already offset within page */
    pass &= test_stp("ptr+16", 16);

    /* Tests 3-5: immediate offset variants on a freshly mapped PROT_NONE page */
    {
        uint8_t *p = mmap(NULL, PAGE, PROT_NONE, MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
        if (p == MAP_FAILED) {
            fprintf(stderr, "stp_test C: mmap failed for imm tests\n");
            return 1;
        }
        pass &= test_stp_imm("imm=16",  p, 16);
        pass &= test_stp_imm("imm=32",  p, 32);
        pass &= test_stp_imm("imm=112", p, 112);
        munmap(p, PAGE);
    }

    if (pass) {
        printf("stp_test C: ALL PASSED\n");
        return 0;
    } else {
        fprintf(stderr, "stp_test C: FAILED\n");
        return 1;
    }
}
