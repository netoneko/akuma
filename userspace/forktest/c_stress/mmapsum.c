/*
 * mmapsum.c — content-integrity check for file-backed mmap vs read().
 *
 * Hashes (FNV-1a 64) the same file four ways and prints each digest:
 *   read:   sequential read() into a buffer         — the known-good VFS path
 *   mmap1:  single-threaded pass over one mapping   — demand-paged content
 *   mmap2:  second pass over the SAME mapping       — resident-page stability
 *   madv:   fresh mapping, madvise(MADV_WILLNEED)   — the pre-fault path
 *           over the whole range, then hashed         (llama.cpp's load shape)
 *   mt:     two threads hashing one half each of a  — concurrent demand paging
 *           FRESH mapping (per-half digests printed)
 *
 * If read != mmap1: the demand-paging fill delivered wrong bytes.
 * If mmap1 != mmap2: resident pages changed under us (eviction/refill bug).
 * If read != madv: the MADV_WILLNEED pre-fault destroyed file content — the
 *   exact bug that made llama.cpp produce garbage with mmap on smp-shared
 *   (sys_madvise installed ZEROED frames for file-backed lazy pages, 2026-07-25).
 * If mt halves differ across runs: corruption needs cross-thread concurrency.
 *
 * Static, musl, pure C — kernel-fault attribution, no runtime suspects.
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mmapsum mmapsum.c
 * Usage: mmapsum <path>
 */

#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static uint64_t fnv1a(const unsigned char *p, size_t n) {
    uint64_t h = 0xcbf29ce484222325ULL;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}

struct half { const unsigned char *base; size_t len; uint64_t hash; };

static void *hash_half(void *arg) {
    struct half *h = arg;
    h->hash = fnv1a(h->base, h->len);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: mmapsum <path>\n");
        return 2;
    }
    const char *path = argv[1];
    int fd = open(path, O_RDONLY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_size <= 0) {
        fprintf(stderr, "mmapsum: open/fstat(%s) failed\n", path);
        return 2;
    }
    size_t size = (size_t)st.st_size;

    /* read() reference, in 1 MiB chunks. */
    unsigned char *buf = malloc(1 << 20);
    uint64_t h = 0xcbf29ce484222325ULL;
    size_t off = 0;
    while (off < size) {
        ssize_t n = pread(fd, buf, 1 << 20, (off_t)off);
        if (n <= 0) { fprintf(stderr, "mmapsum: pread failed at %zu\n", off); return 2; }
        for (ssize_t i = 0; i < n; i++) { h ^= buf[i]; h *= 0x100000001b3ULL; }
        off += (size_t)n;
    }
    printf("read:  %016llx\n", (unsigned long long)h);
    fflush(stdout);

    /* Single-threaded mapping, hashed twice. */
    unsigned char *m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) { fprintf(stderr, "mmapsum: mmap failed\n"); return 2; }
    printf("mmap1: %016llx\n", (unsigned long long)fnv1a(m, size));
    fflush(stdout);
    printf("mmap2: %016llx\n", (unsigned long long)fnv1a(m, size));
    fflush(stdout);
    munmap(m, size);

    /* Fresh mapping, MADV_WILLNEED pre-fault over the whole range, then hash —
     * llama.cpp's mmap loader does exactly this before touching the weights. */
    m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) { fprintf(stderr, "mmapsum: mmap#madv failed\n"); return 2; }
    (void)madvise(m, size, MADV_WILLNEED);
    printf("madv:  %016llx\n", (unsigned long long)fnv1a(m, size));
    fflush(stdout);
    munmap(m, size);

    /* Fresh mapping, two threads demand-paging concurrently (one half each). */
    m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) { fprintf(stderr, "mmapsum: mmap#2 failed\n"); return 2; }
    struct half a = { m, size / 2, 0 };
    struct half b = { m + size / 2, size - size / 2, 0 };
    pthread_t ta, tb;
    pthread_create(&ta, NULL, hash_half, &a);
    pthread_create(&tb, NULL, hash_half, &b);
    pthread_join(ta, NULL);
    pthread_join(tb, NULL);
    printf("mtA:   %016llx\nmtB:   %016llx\n",
           (unsigned long long)a.hash, (unsigned long long)b.hash);
    munmap(m, size);
    close(fd);
    return 0;
}
