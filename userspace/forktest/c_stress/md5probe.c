// md5probe — a static, non-busybox MD5. The missing cell in the elimination
// table of docs/archive/BUSYBOX_HASH_MISCOMPUTE.md.
//
// Established so far: the bytes are provably correct (read and mmap both verify
// byte-exact), GPR/D/Q compute is stable, and yet md5sum/sha1sum/sha512sum are
// wrong ~50% of the time while cksum and base64 on the same file are stable.
// Two independent busybox builds (ours static, Alpine's dynamic PIE) both fail,
// so it is not the binary. What has never been tested is a *different* md5
// implementation: everything failing so far is libbb's hash driver.
//
//   * If this probe ALSO miscomputes -> the defect is not busybox at all; it is
//     md5-shaped computation in the guest, and the next step is to bisect which
//     part (the 64-byte block buffer, the 4-word state, the constant table).
//   * If this probe is CLEAN -> the defect is specific to libbb's driver, and
//     the difference between it and this file is the whole remaining search
//     space.
//
// Structure deliberately mirrors busybox's: read the file in `chunk`-sized
// pieces and fold each piece into a context that lives across the read(2).
//
// `file` mode hashes the SAME file `iters` times inside ONE process and prints
// every digest, because the signature to reproduce is "first pass wrong,
// subsequent passes right".
//
// Calibrated: compile natively and check against a known digest before trusting
// any guest reading. A wrong digest everywhere is a bug in this file.
//
// Build (guest):  aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o md5probe md5probe.c
// Build (host):   cc -O2 -o md5probe_host md5probe.c
// Usage: md5probe file <path> [iters] [chunk]
//        md5probe mem  [size] [iters]

#define _GNU_SOURCE
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>

typedef struct {
    uint32_t s[4];
    uint64_t total;
    uint8_t  blk[64];
    size_t   blen;
} md5_ctx;

static const uint32_t K[64] = {
    0xd76aa478,0xe8c7b756,0x242070db,0xc1bdceee,0xf57c0faf,0x4787c62a,0xa8304613,0xfd469501,
    0x698098d8,0x8b44f7af,0xffff5bb1,0x895cd7be,0x6b901122,0xfd987193,0xa679438e,0x49b40821,
    0xf61e2562,0xc040b340,0x265e5a51,0xe9b6c7aa,0xd62f105d,0x02441453,0xd8a1e681,0xe7d3fbc8,
    0x21e1cde6,0xc33707d6,0xf4d50d87,0x455a14ed,0xa9e3e905,0xfcefa3f8,0x676f02d9,0x8d2a4c8a,
    0xfffa3942,0x8771f681,0x6d9d6122,0xfde5380c,0xa4beea44,0x4bdecfa9,0xf6bb4b60,0xbebfbc70,
    0x289b7ec6,0xeaa127fa,0xd4ef3085,0x04881d05,0xd9d4d039,0xe6db99e5,0x1fa27cf8,0xc4ac5665,
    0xf4292244,0x432aff97,0xab9423a7,0xfc93a039,0x655b59c3,0x8f0ccc92,0xffeff47d,0x85845dd1,
    0x6fa87e4f,0xfe2ce6e0,0xa3014314,0x4e0811a1,0xf7537e82,0xbd3af235,0x2ad7d2bb,0xeb86d391,
};
static const uint8_t R[64] = {
    7,12,17,22,7,12,17,22,7,12,17,22,7,12,17,22,
    5, 9,14,20,5, 9,14,20,5, 9,14,20,5, 9,14,20,
    4,11,16,23,4,11,16,23,4,11,16,23,4,11,16,23,
    6,10,15,21,6,10,15,21,6,10,15,21,6,10,15,21,
};

static uint32_t rol(uint32_t x, uint32_t c) { return (x << c) | (x >> (32 - c)); }

static void md5_block(md5_ctx *c, const uint8_t *p)
{
    uint32_t m[16];
    for (int i = 0; i < 16; i++)
        m[i] = (uint32_t)p[i*4] | ((uint32_t)p[i*4+1] << 8)
             | ((uint32_t)p[i*4+2] << 16) | ((uint32_t)p[i*4+3] << 24);
    uint32_t a = c->s[0], b = c->s[1], cc = c->s[2], d = c->s[3];
    for (uint32_t i = 0; i < 64; i++) {
        uint32_t f, g;
        if (i < 16)      { f = (b & cc) | (~b & d);      g = i; }
        else if (i < 32) { f = (d & b) | (~d & cc);      g = (5*i + 1) & 15; }
        else if (i < 48) { f = b ^ cc ^ d;               g = (3*i + 5) & 15; }
        else             { f = cc ^ (b | ~d);            g = (7*i) & 15; }
        uint32_t tmp = d;
        d = cc; cc = b;
        b = b + rol(a + f + K[i] + m[g], R[i]);
        a = tmp;
    }
    c->s[0] += a; c->s[1] += b; c->s[2] += cc; c->s[3] += d;
}

static void md5_init(md5_ctx *c)
{
    c->s[0] = 0x67452301; c->s[1] = 0xefcdab89;
    c->s[2] = 0x98badcfe; c->s[3] = 0x10325476;
    c->total = 0; c->blen = 0;
}

static void md5_update(md5_ctx *c, const uint8_t *p, size_t n)
{
    c->total += n;
    if (c->blen) {
        size_t need = 64 - c->blen;
        if (n < need) { memcpy(c->blk + c->blen, p, n); c->blen += n; return; }
        memcpy(c->blk + c->blen, p, need);
        md5_block(c, c->blk);
        p += need; n -= need; c->blen = 0;
    }
    while (n >= 64) { md5_block(c, p); p += 64; n -= 64; }
    if (n) { memcpy(c->blk, p, n); c->blen = n; }
}

static void md5_final(md5_ctx *c, uint8_t out[16])
{
    uint64_t bits = c->total * 8;
    uint8_t pad = 0x80;
    md5_update(c, &pad, 1);
    // md5_update bumped total; that is fine, length was captured above.
    while (c->blen != 56) { uint8_t z = 0; md5_update(c, &z, 1); }
    uint8_t lb[8];
    for (int i = 0; i < 8; i++) lb[i] = (uint8_t)(bits >> (8*i));
    md5_update(c, lb, 8);
    for (int i = 0; i < 4; i++) {
        out[i*4+0] = (uint8_t)(c->s[i]);
        out[i*4+1] = (uint8_t)(c->s[i] >> 8);
        out[i*4+2] = (uint8_t)(c->s[i] >> 16);
        out[i*4+3] = (uint8_t)(c->s[i] >> 24);
    }
}

// busybox's md5sum issues rt_sigaction x2 (measured from the kernel side via
// [PSTATS]); this probe issued none. That is the last structural difference
// between it (always clean) and busybox (~50% wrong), and it is a load-bearing
// one: a process with an installed handler gets a signal FRAME WRITTEN ONTO ITS
// USER STACK, which is exactly where md5's context and 64-byte block buffer
// live. `cksum`/`base64` — stable in the same binary — keep their state in a
// register or a tiny struct. And a stale pending signal delivered early in a
// freshly-exec'd process would explain the "first file wrong, second right"
// signature exactly.
static volatile sig_atomic_t g_handler_runs = 0;
static void noop_handler(int sig) { (void)sig; g_handler_runs++; }

static void install_handlers(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = noop_handler;
    // A spread of signals whose default action would NOT kill us, so the only
    // observable difference from the no-handler build is that a delivery now
    // builds a frame on our stack instead of being discarded.
    int sigs[] = { SIGCHLD, SIGWINCH, SIGURG, SIGCONT, SIGUSR1, SIGUSR2, SIGPIPE };
    for (unsigned i = 0; i < sizeof sigs / sizeof sigs[0]; i++)
        sigaction(sigs[i], &sa, NULL);
}

static void hex(const uint8_t d[16], char out[33])
{
    static const char *h = "0123456789abcdef";
    for (int i = 0; i < 16; i++) { out[i*2] = h[d[i] >> 4]; out[i*2+1] = h[d[i] & 15]; }
    out[32] = 0;
}

// `hole <path> <page> <iters>`: the falsification test for "a fault DURING the
// copy loses bytes". The destination is one page-aligned mapping, fully touched
// so no page can fault — except ONE page, which is replaced by a fresh
// MAP_FIXED anonymous page and left untouched. The single 64 KB read then has
// exactly one possible fault, at a known page.
//
// Prediction if the race is real: the corrupted bytes sit at or immediately
// around that page, and moving the hole moves the damage. If corruption appears
// far from the hole, or with no hole at all, the mechanism is something else.
static int hole_mode(const char *path, int hole_count, int iters)
{
    struct stat st;
    int bad_runs = 0;
    for (int k = 0; k < iters; k++) {
        int fd = open(path, O_RDONLY);
        if (fd < 0) { perror("open"); return 2; }
        if (fstat(fd, &st) != 0) { perror("fstat"); return 2; }
        size_t sz = (size_t)st.st_size;
        uint8_t *b = mmap(NULL, sz, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (b == MAP_FAILED) { perror("mmap"); return 2; }
        memset(b, 0xEE, sz);                    /* fault EVERY page in */
        // Then punch `hole_count` pages back to fresh, untouched mappings, so the
        // number of pages that can fault during the copy is exactly controlled.
        if (hole_count == -2) {
            // ONE MAP_FIXED over the whole range: a single large lazy region,
            // instead of `hole_count` separate one-page regions. That is the only
            // structural difference left between the clean per-page arms and the
            // corrupting `whole`/`whole-mmap` arms, so it is the discriminator.
            if (mmap(b, sz, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == MAP_FAILED) {
                perror("mmap FIXED whole"); return 2;
            }
        } else for (int h = 0; h < hole_count; h++) {
            void *hp = b + (size_t)h * 4096;
            if (mmap(hp, 4096, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == MAP_FAILED) {
                perror("mmap FIXED hole"); return 2;
            }
        }
        // CONTROL: the experiment is only meaningful if a punched page really is
        // a fresh, unpopulated mapping. A fresh anon page reads as 0; if it still
        // reads 0xEE, MAP_FIXED kept the old frame and there is no hole at all —
        // in which case every "clean" reading above proves nothing.
        if (hole_count != 0 && k == 0) {
            unsigned char first_byte = b[0];
            printf("  [control] byte 0 of punched page reads %#x -> %s\n",
                   first_byte,
                   first_byte == 0 ? "FRESH page (hole is real)"
                                   : "STALE 0xEE (MAP_FIXED kept the frame; NO HOLE)");
        }
        size_t got = 0;
        while (got < sz) {
            ssize_t n = read(fd, b + got, sz - got);
            if (n <= 0) break;
            got += (size_t)n;
        }
        close(fd);
        long bad = 0; size_t lo = 0, hi = 0;
        for (size_t o = 0; o + 4 <= got; o += 4) {
            uint32_t want = (uint32_t)o, g;
            memcpy(&g, b + o, 4);
            if (g != want) { if (!bad) lo = o; hi = o; bad++; }
        }
        if (bad) {
            bad_runs++;
            printf("  run %d holes=%d: %ld bad words, span [%zu..%zu] "
                   "= pages %zu..%zu\n", k, hole_count, bad, lo, hi, lo/4096, hi/4096);
        }
        munmap(b, sz);
    }
    printf("holes=%2d: %d/%d runs corrupted\n", hole_count, bad_runs, iters);
    return 0;
}

int main(int argc, char **argv)
{
    const char *mode = (argc > 1) ? argv[1] : "file";
    // `SIGH=1` in the environment installs handlers before any hashing.
    const char *sigh = getenv("SIGH");
    if (sigh && sigh[0] == '1') install_handlers();

    if (strcmp(mode, "mem") == 0) {
        size_t size  = (argc > 2) ? (size_t)strtoul(argv[2], NULL, 10) : 65536;
        int    iters = (argc > 3) ? atoi(argv[3]) : 200;
        uint8_t *buf = malloc(size);
        if (!buf) { perror("malloc"); return 2; }
        for (size_t i = 0; i < size; i += 4) {
            uint32_t w = (uint32_t)i;
            size_t n = (size - i < 4) ? size - i : 4;
            memcpy(buf + i, &w, n);
        }
        char first[33] = {0}; int diff = 0;
        for (int k = 0; k < iters; k++) {
            md5_ctx c; md5_init(&c);
            md5_update(&c, buf, size);
            uint8_t d[16]; md5_final(&c, d);
            char s[33]; hex(d, s);
            if (!k) memcpy(first, s, 33);
            else if (strcmp(first, s) != 0) { if (!diff) printf("  differs at iter %d: %s\n", k, s); diff++; }
        }
        printf("mem: size=%zu iters=%d digest=%s differing=%d\n", size, iters, first, diff);
        printf("RESULT: %s\n", diff ? "FAIL" : "PASS");
        return diff ? 1 : 0;
    }

    // `whole` mode: busybox's measured shape, as closely as possible — fstat the
    // file, allocate a buffer of exactly that size, and read it ALL in ONE call
    // into that untouched allocation, then hash. [PSTATS] showed busybox issuing
    // fstatat and exactly one read for a 64 KB file, and a fresh malloc is the
    // one destination class this probe has never used untouched (earlier arms
    // memset it first, or used mmap/stack/.bss).
    // `whole-touch` is `whole` with ONE line added: the buffer is memset before
    // the read, which faults its pages in. If touching fixes it, the defect is
    // read(2)/copy_to_user writing into lazily-mapped brk-heap destination pages
    // — and that is also why every earlier arm of every probe was clean: they all
    // either memset first or used mmap.
    // `whole-mmap` is `whole` with the malloc replaced by an anonymous mapping,
    // to confirm the heap is the differentiator and not the size/shape.
    if (strcmp(mode, "whole") == 0 || strcmp(mode, "whole-touch") == 0
        || strcmp(mode, "whole-mmap") == 0 || strcmp(mode, "whole-mmap-off") == 0
        || strcmp(mode, "whole-warm") == 0) {
        int touch = (strcmp(mode, "whole-touch") == 0);
        int use_mmap = (strcmp(mode, "whole-mmap") == 0);
        // `whole-mmap-off`: page-aligned mapping, but the read destination is
        // deliberately offset by 64 bytes, mimicking malloc's non-page-aligned
        // pointer. If THIS fails while `whole-mmap` passes, the differentiator is
        // destination ALIGNMENT across lazily-mapped pages, not brk-vs-mmap
        // (musl's mallocng serves a 64 KB request from mmap anyway, so there is
        // no brk involved in either case).
        int off_mmap = (strcmp(mode, "whole-mmap-off") == 0);
        if (off_mmap) use_mmap = 1;
        // `whole-warm`: identical to `whole`, but does one throwaway
        // mmap+memset+munmap FIRST. Every hole-mode arm (all clean) happened to
        // do exactly that kind of setup before its read, while `whole` (corrupts
        // ~50%) reads almost immediately after execve. If warming makes the
        // corruption vanish, the trigger is fresh-address-space state, not page
        // laziness — the hole arms would then have been testing the wrong thing.
        if (strcmp(mode, "whole-warm") == 0) {
            void *w = mmap(NULL, 1u << 20, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (w != MAP_FAILED) { memset(w, 0x5A, 1u << 20); munmap(w, 1u << 20); }
        }
        const char *wp = (argc > 2) ? argv[2] : "/tmp/ident.bin";
        int iters = (argc > 3) ? atoi(argv[3]) : 10;
        char first[33] = {0}; int diff = 0;
        for (int k = 0; k < iters; k++) {
            struct stat st;
            int fd = open(wp, O_RDONLY);
            if (fd < 0) { perror("open"); return 2; }
            if (fstat(fd, &st) != 0) { perror("fstat"); return 2; }
            size_t sz = (size_t)st.st_size;
            uint8_t *b;
            uint8_t *mbase = NULL;
            if (use_mmap) {
                size_t msz = sz + 4096;
                mbase = mmap(NULL, msz, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (mbase == MAP_FAILED) { perror("mmap"); return 2; }
                b = off_mmap ? mbase + 64 : mbase;
            } else {
                b = malloc(sz);               /* deliberately NOT touched */
                if (!b) { perror("malloc"); return 2; }
            }
            if (touch) memset(b, 0, sz);      /* fault the destination in first */
            size_t got = 0;
            while (got < sz) {
                ssize_t n = read(fd, b + got, sz - got);
                if (n <= 0) break;
                got += (size_t)n;
            }
            close(fd);
            md5_ctx c; md5_init(&c); md5_update(&c, b, got);
            uint8_t d[16]; md5_final(&c, d);
            char h[33]; hex(d, h);
            // The file is self-identifying: the 4-byte word at offset o holds o.
            // So name exactly which bytes are wrong and where their data came
            // from — that turns "wrong digest" into a mechanism.
            long bad = 0; size_t first_bad = 0, last_bad = 0; uint32_t fw = 0, fg = 0;
            long per_page[64] = {0};
            long marker_words = 0;   /* how many hold the kernel's fresh-page marker */
            for (size_t o = 0; o + 4 <= got; o += 4) {
                uint32_t want = (uint32_t)o, gotw;
                memcpy(&gotw, b + o, 4);
                if (gotw != want) {
                    if (!bad) { first_bad = o; fw = want; fg = gotw; }
                    last_bad = o;
                    bad++;
                    if (o / 4096 < 64) per_page[o / 4096]++;
                    if (gotw == 0xDEADF00Du) marker_words++;
                }
            }
            printf("  iter %d: %s (read %zu of %zu, st_size=%lld) badwords=%ld\n",
                   k, h, got, sz, (long long)st.st_size, bad);
            if (bad) {
                long span_words = (long)((last_bad - first_bad) / 4) + 1;
                printf("      bad span [%zu..%zu] = %ld words over %ld word-slots -> %s\n",
                       first_bad, last_bad, bad, span_words,
                       (bad == span_words) ? "CONTIGUOUS" : "SCATTERED");
                printf("      per-page bad words:");
                for (int pg = 0; pg < 17; pg++)
                    if (per_page[pg]) printf(" p%d=%ld", pg, per_page[pg]);
                printf("\n");
                printf("      words holding the 0xDEADF00D fresh-page marker: %ld of %ld"
                       " -> %s\n", marker_words, bad,
                       marker_words == bad ? "NEVER WRITTEN by the copy"
                                           : (marker_words ? "partly never written" : "written, then wrong"));
                printf("      first bad at offset %zu (page %zu, in-page %zu): want %u got %u\n",
                       first_bad, first_bad / 4096, first_bad % 4096, fw, fg);
                if (fg == 0) printf("      -> ZEROES (destination page never received the data)\n");
                else if ((fg % 4) == 0 && (size_t)fg + 4 <= got) {
                    long dl = (long)fg - (long)first_bad;
                    printf("      -> data belonging at offset %u, delta %+ld%s\n", fg, dl,
                           (dl % 4096 == 0) ? " (PAGE ALIASING)" : "");
                } else printf("      -> foreign data: %#x\n", fg);
                // The damage is not one window: it repeats at the SAME
                // in-page offsets on consecutive pages. So report the histogram
                // of in-page offsets over ALL bad words — that names the shape
                // in one line — plus the raw values on the first two pages.
                printf("      dst=%p (page offset %#zx)\n", (void *)b, (size_t)b & 0xFFF);
                {
                    static int hist[1024];
                    memset(hist, 0, sizeof hist);
                    for (size_t o = 0; o + 4 <= got; o += 4) {
                        uint32_t want2 = (uint32_t)o, g2;
                        memcpy(&g2, b + o, 4);
                        if (g2 != want2) hist[(((size_t)b + o) & 0xFFF) / 4]++;
                    }
                    printf("      bad in-page offsets (offset:pages):");
                    for (int i = 0; i < 1024; i++)
                        if (hist[i]) printf(" %d:%d", i * 4, hist[i]);
                    printf("\n");
                    size_t page0 = ((size_t)b + 0xFFF) & ~(size_t)0xFFF;
                    for (int pg = 0; pg < 2; pg++) {
                        size_t pv = page0 + (size_t)pg * 4096;
                        if (pv + 64 > (size_t)b + got) break;
                        printf("      page %#zx in-page 0..63:", pv >> 12);
                        for (int j = 0; j < 8; j++) {
                            uint64_t v; memcpy(&v, (char *)pv + j * 8, 8);
                            printf(" %llx", (unsigned long long)v);
                        }
                        printf("\n");
                    }
                }
            }
            if (!k) memcpy(first, h, 33);
            else if (strcmp(first, h) != 0) diff++;
            if (use_mmap) munmap(mbase, sz + 4096); else free(b);
        }
        printf("%s: %s iters=%d differing_from_first=%d\n", mode, wp, iters, diff);
        return 0;
    }

    if (strcmp(mode, "hole") == 0) {
        const char *hp = (argc > 2) ? argv[2] : "/tmp/ident.bin";
        int page = (argc > 3) ? atoi(argv[3]) : 0;   /* now a COUNT */
        int it   = (argc > 4) ? atoi(argv[4]) : 10;
        return hole_mode(hp, page, it);
    }

    const char *path = (argc > 2) ? argv[2] : "/tmp/ident.bin";
    int    iters = (argc > 3) ? atoi(argv[3]) : 8;
    size_t chunk = (argc > 4) ? (size_t)strtoul(argv[4], NULL, 10) : 4096;
    uint8_t *buf = malloc(chunk);
    if (!buf) { perror("malloc"); return 2; }

    char first[33] = {0}; int diff = 0;
    for (int k = 0; k < iters; k++) {
        int fd = open(path, O_RDONLY);
        if (fd < 0) { perror("open"); return 2; }
        md5_ctx c; md5_init(&c);
        for (;;) {
            ssize_t n = read(fd, buf, chunk);
            if (n < 0) { perror("read"); close(fd); return 2; }
            if (n == 0) break;
            md5_update(&c, buf, (size_t)n);
        }
        close(fd);
        uint8_t d[16]; md5_final(&c, d);
        char s[33]; hex(d, s);
        printf("  iter %d: %s\n", k, s);
        if (!k) memcpy(first, s, 33);
        else if (strcmp(first, s) != 0) diff++;
    }
    printf("file: %s iters=%d chunk=%zu differing_from_first=%d handler_runs=%d\n",
           path, iters, chunk, diff, (int)g_handler_runs);
    return 0;
}
