// readback — localize the non-deterministic read corruption to an offset.
//
// Background: busybox `md5sum` returns wrong, non-deterministic digests for an
// unmodified file >4096 bytes, while `cat` returned byte-perfect data. A digest
// tells you only "something differed"; it cannot say WHERE or WHAT. An
// in-memory compute probe (computecheck) ruled out register/FP corruption
// across preemption: 0/400 wrong. So the bytes themselves are suspect, and the
// question is which ones.
//
// The file this probe writes is SELF-IDENTIFYING: the 4-byte word at byte
// offset `o` contains `o` itself, little-endian. So a mismatch does not just
// say "wrong" — it says "at offset X we were handed the data that belongs at
// offset Y". Y-X is the whole diagnosis:
//   * Y-X a multiple of 4096  -> page aliasing (wrong page in the cache)
//   * Y-X small/unaligned     -> a short/shifted copy (copy_to_user length bug)
//   * Y garbage (not a valid  -> foreign data: another file, or freed memory
//     offset in this file)
//
// Reads are issued in a configurable chunk size because the reproduction is
// size-sensitive (>4096 only), and busybox's applets differ from each other
// exactly in their buffer shape.
//
// Calibrated: this same static binary must report 0 mismatches on real Linux
// arm64. A failure there is a probe bug, not a kernel bug.
//
// Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o readback readback.c
// Usage: readback <path> <size_bytes> <iters> <chunk_bytes>

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

// Destination buffer in .bss: lazily zero-filled by the ELF loader, and per
// `cowstale`/`bssfork` history the .data/.bss range has no region record backing
// its fault judgement. Never touched before the read, and only meaningful with
// ONE iteration per fresh process.
static unsigned char g_bss_buf[128 * 1024];

static uint32_t expected_word(size_t off) { return (uint32_t)off; }

// FNV-1a, incremental. The point is WHERE it runs: folded into the read loop, so
// the accumulator has to survive every read(2) in between — exactly busybox
// md5sum's shape, and the one thing the earlier compute probes never tested
// (they hashed a warm buffer with no syscalls inside the loop). A run where the
// buffer memcmps byte-exact but this digest is wrong means the BYTES were fine
// and the in-register/in-stack accumulator was corrupted across a syscall.
static uint64_t fnv_step(uint64_t h, const unsigned char *p, size_t n)
{
    for (size_t i = 0; i < n; i++) { h ^= p[i]; h *= 1099511628211ULL; }
    return h;
}
#define FNV_INIT 1469598103934665603ULL

int main(int argc, char **argv)
{
    // `verify` mode: do NOT write the file, only check an existing one against
    // the offset pattern. This is how the same bytes get read two ways — by this
    // probe and by busybox md5sum — so a disagreement says which of the two is
    // wrong. It also lets the file arrive by a DIFFERENT path (pushed in over
    // ssh, i.e. written by sshd) than the process reading it, which is the shape
    // the original reproduction had and a self-written file does not.
    int verify_only = (argc > 1 && strcmp(argv[1], "verify") == 0);
    if (verify_only) { argv++; argc--; }
    // `stackverify`: same as `verify`, but the read buffer AND the running
    // state live on the STACK, not the heap. This is the last structural
    // difference between this probe (heap buffer, accumulator in a register —
    // always clean) and busybox md5sum (context + buffer on the stack — wrong
    // ~50% of invocations). If the stack arm corrupts where the heap arm does
    // not, the defect is the kernel writing into live user stack memory.
    int stack_mode = (argc > 1 && strcmp(argv[1], "stackverify") == 0);
    if (stack_mode) { argv++; argc--; verify_only = 1; }
    // `freshbuf`: read into a brand-new anonymous mapping that is NEVER touched
    // before the read. Every earlier arm memset the destination first, which
    // faults the whole buffer in and — as it turns out — hides the defect:
    // busybox md5sum gets the FIRST file of a fresh process wrong and the
    // second right, so the suspect is read(2) copying into pages that are still
    // lazily unmapped.
    int fresh_mode = (argc > 1 && strcmp(argv[1], "freshbuf") == 0);
    if (fresh_mode) { argv++; argc--; verify_only = 1; }
    int bss_mode = (argc > 1 && strcmp(argv[1], "bssverify") == 0);
    if (bss_mode) { argv++; argc--; verify_only = 1; }
    const char *path = (argc > 1) ? argv[1] : "/tmp/readback.bin";
    size_t size  = (argc > 2) ? (size_t)strtoul(argv[2], NULL, 10) : 65536;
    int    iters = (argc > 3) ? atoi(argv[3]) : 50;
    size_t chunk = (argc > 4) ? (size_t)strtoul(argv[4], NULL, 10) : 4096;

    size = size & ~(size_t)3;   // whole words only
    // 128 KiB of stack, deliberately: big enough to span many pages so a stray
    // kernel write into the stack region is likely to land inside it.
    unsigned char stack_buf[128 * 1024];
    unsigned char *ref = malloc(size);
    unsigned char *got = stack_mode ? stack_buf : (bss_mode ? g_bss_buf : malloc(size));
    if (!ref || !got) { perror("malloc"); return 2; }
    if (stack_mode && size > sizeof stack_buf) {
        printf("size %zu exceeds stack buffer %zu\n", size, sizeof stack_buf);
        return 2;
    }

    for (size_t o = 0; o + 4 <= size; o += 4) {
        uint32_t w = expected_word(o);
        memcpy(ref + o, &w, 4);
    }

    // Write it once, and get it onto disk so nothing below depends on a dirty
    // page still being in the cache.
    int fd;
    size_t done = 0;
    if (!verify_only) {
        fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
        if (fd < 0) { perror("open w"); return 2; }
        while (done < size) {
            ssize_t n = write(fd, ref + done, size - done);
            if (n <= 0) { perror("write"); return 2; }
            done += (size_t)n;
        }
        if (fsync(fd) != 0) perror("fsync (non-fatal)");
        close(fd);
    }

    int bad_iters = 0;
    int bad_hash = 0;
    long total_bad_words = 0;
    uint64_t want_hash = fnv_step(FNV_INIT, ref, size);
    uint64_t first_bad_hash = 0;
    // Histogram of (source - target) deltas, in pages, for the first few finds.
    int reported = 0;

    for (int k = 0; k < iters; k++) {
        fd = open(path, O_RDONLY);
        if (fd < 0) { perror("open r"); return 2; }
        if (bss_mode) {
            // Deliberately do NOT pre-touch: the first write to these pages must
            // be read(2)'s own copy_to_user.
        } else if (fresh_mode) {
            // Fresh, untouched, lazily-backed destination for every iteration.
            got = mmap(NULL, size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (got == MAP_FAILED) { perror("mmap anon"); return 2; }
        } else {
            memset(got, 0xAA, size);
        }
        done = 0;
        int short_read = 0;
        uint64_t h = FNV_INIT;
        while (done < size) {
            size_t want = size - done;
            if (want > chunk) want = chunk;
            ssize_t n = read(fd, got + done, want);
            if (n < 0) { perror("read"); close(fd); return 2; }
            if (n == 0) { short_read = 1; break; }
            // Hash this chunk BEFORE the next read, so `h` is live across it.
            h = fnv_step(h, got + done, (size_t)n);
            done += (size_t)n;
        }
        close(fd);
        if (!short_read && done == size && h != want_hash) {
            if (!bad_hash) first_bad_hash = h;
            bad_hash++;
        }

        if (short_read || done != size) {
            printf("iter %d: SHORT READ, got %zu of %zu bytes\n", k, done, size);
            bad_iters++;
            continue;
        }
        if (memcmp(ref, got, size) == 0) {
            if (fresh_mode) munmap(got, size);
            continue;
        }

        bad_iters++;
        long bad_words = 0;
        size_t first_off = 0;
        uint32_t first_want = 0, first_got = 0;
        for (size_t o = 0; o + 4 <= size; o += 4) {
            uint32_t w_ref, w_got;
            memcpy(&w_ref, ref + o, 4);
            memcpy(&w_got, got + o, 4);
            if (w_ref != w_got) {
                if (bad_words == 0) { first_off = o; first_want = w_ref; first_got = w_got; }
                bad_words++;
            }
        }
        total_bad_words += bad_words;

        if (reported < 6) {
            reported++;
            printf("iter %d: %ld wrong word(s) of %zu\n", k, bad_words, size / 4);
            printf("    first at offset %zu (page %zu, offset-in-page %zu)\n",
                   first_off, first_off / 4096, first_off % 4096);
            printf("    expected word %u (%#x), got %u (%#x)\n",
                   first_want, first_want, first_got, first_got);
            // Interpret the value we got: is it a valid offset in this file?
            if ((first_got % 4) == 0 && (size_t)first_got + 4 <= size) {
                long delta = (long)first_got - (long)first_off;
                printf("    -> got the data belonging at offset %u; delta = %+ld",
                       first_got, delta);
                if (delta % 4096 == 0)
                    printf(" (= %+ld page(s): PAGE ALIASING)\n", delta / 4096);
                else
                    printf(" (not a page multiple: shifted/short copy)\n");
            } else {
                printf("    -> value is NOT a valid offset in this file: foreign data\n");
            }
        }
    }

    printf("readback: path=%s size=%zu chunk=%zu iters=%d\n", path, size, chunk, iters);
    // ---- mmap arm -------------------------------------------------------
    // Most md5sum implementations mmap a regular file rather than read() it,
    // and the corruption's one-page threshold is exactly where such a cutoff
    // sits. Same file, same expected bytes, different kernel path.
    int bad_mmap = 0;
    size_t mm_first_off = 0; uint32_t mm_want = 0, mm_got = 0;
    for (int k = 0; k < iters; k++) {
        int mfd = open(path, O_RDONLY);
        if (mfd < 0) { perror("open mmap"); break; }
        void *m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, mfd, 0);
        close(mfd);
        if (m == MAP_FAILED) { perror("mmap"); break; }
        if (memcmp(ref, m, size) != 0) {
            if (!bad_mmap) {
                for (size_t o = 0; o + 4 <= size; o += 4) {
                    uint32_t a, b;
                    memcpy(&a, ref + o, 4);
                    memcpy(&b, (const unsigned char *)m + o, 4);
                    if (a != b) { mm_first_off = o; mm_want = a; mm_got = b; break; }
                }
            }
            bad_mmap++;
        }
        munmap(m, size);
    }

    printf("  bad iterations (bytes) : %d / %d\n", bad_iters, iters);
    printf("  total wrong words      : %ld\n", total_bad_words);
    printf("  wrong incremental hash : %d / %d\n", bad_hash, iters);
    printf("  bad iterations (mmap)  : %d / %d\n", bad_mmap, iters);
    if (bad_mmap) {
        printf("    first wrong at offset %zu (page %zu, in-page %zu)\n",
               mm_first_off, mm_first_off / 4096, mm_first_off % 4096);
        printf("    expected %u (%#x), got %u (%#x)\n", mm_want, mm_want, mm_got, mm_got);
        if ((mm_got % 4) == 0 && (size_t)mm_got + 4 <= size) {
            long d = (long)mm_got - (long)mm_first_off;
            printf("    -> data belonging at offset %u; delta %+ld%s\n", mm_got, d,
                   (d % 4096 == 0) ? " (PAGE ALIASING)" : " (shifted copy)");
        } else if (mm_got == 0) {
            printf("    -> ZEROES: an unpopulated/zero-filled file page\n");
        } else {
            printf("    -> foreign data (not an offset in this file)\n");
        }
    }
    if (bad_mmap && !bad_iters)
        printf("  VERDICT: read() exact but mmap wrong -> the file-page mapping "
               "path is the defect, not the read path\n");
    if (bad_hash)
        printf("    want=%016llx first_wrong=%016llx\n",
               (unsigned long long)want_hash, (unsigned long long)first_bad_hash);
    if (bad_hash && !bad_iters)
        printf("  VERDICT: bytes exact, hash wrong -> accumulator corrupted "
               "ACROSS read(2), not a data problem\n");
    int any = bad_iters || bad_hash || bad_mmap;
    printf("RESULT: %s\n", any ? "FAIL" : "PASS (read, incremental hash and mmap all exact)");
    return any ? 1 : 0;
}
