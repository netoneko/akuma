/*
 * abi_write_probe — every syscall that serializes a kernel struct into a user
 * buffer, checked byte for byte.
 *
 * Why this family: the syscalls below share one shape — the kernel builds a
 * `repr(C)` record in kernel memory and hands it to userspace — and in
 * `src/syscall/` each of them used to build that record through raw pointer
 * arithmetic (`ptr::write` of a `cast::<u64>()` over a `Vec<u8>`,
 * `copy_nonoverlapping` between two fixed arrays, `(*ring_kva).field = ...`).
 * Those were rewritten as safe slice writes on 2026-08-31. A serialization bug
 * introduced there is silent: the syscall still returns 0 and the wrong bytes
 * only surface much later, in a libc that trusted them. So the invariant needs
 * an instrument that reads the raw bytes rather than a libc wrapper's
 * interpretation of them — `readdir()` will happily hide a wrong `d_off`.
 *
 * Two of the rewrites also fixed genuine unaligned accesses: `getdents64` and
 * `sched_getaffinity` both wrote a `u64` through a pointer derived from a
 * `Vec<u8>` (1-aligned) using the *aligned* `ptr::write`. AArch64 tolerates
 * that for normal memory, which is exactly why it never showed up.
 *
 * Built static-musl for BOTH kernels from this one source, like its siblings
 * under `userspace/{mem,futex,epoll}probe/c/`: run it on Linux to get the
 * reference bytes, run the same binary on Akuma, diff the two reports. Anything
 * Akuma is known to spell differently is marked DIVERGE and is not a failure.
 *
 *   termios     TCGETS/TCSETS round-trip, including cc[19] — the last byte of
 *               the 20-byte cc[] array that used to be copied with an explicit
 *               length of 20 next to a separately-written `[u32; 9]` buffer.
 *               Needs a tty; prints `skip` and does not fail without one.
 *   eventfd     write(8)/read(8) of a byte pattern with a bit set in every
 *               octet, so a byte-order or truncation slip cannot round-trip.
 *   aio_ring    io_setup()'s `struct aio_ring` header, read straight out of the
 *               mapped ring the way glibc reads it before trusting head/tail.
 *   affinity    sched_getaffinity()'s mask, into a buffer pre-filled with 0xAA
 *               so a short write is visible as leftover poison.
 *   getdents64  the raw record stream: reclen alignment and bounds, the NUL
 *               terminator, and that the pad to reclen is zero.
 *
 * Build:  userspace/abiprobe/c/build.sh
 * Run:    /tmp/abi_write_probe            (add -v to dump raw bytes)
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <termios.h>
#include <sys/syscall.h>
#include <stddef.h>

static int fails, checks, skips, verbose;

static void ck(int ok, const char *what)
{
    checks++;
    if (ok) { printf("  ok      %s\n", what); }
    else    { printf("  FAIL    %s\n", what); fails++; }
}
static void skip(const char *what, const char *why)
{
    skips++;
    printf("  skip    %s (%s)\n", what, why);
}

/* ---------------------------------------------------------------- termios */
/* Opens the first fd that is actually a tty, or -1. */
static int open_tty(void)
{
    static const char *paths[] = { "/dev/tty", "/dev/console", NULL };
    for (int i = 0; i < 3; i++)
        if (isatty(i)) return dup(i);
    for (int i = 0; paths[i]; i++) {
        int fd = open(paths[i], O_RDWR | O_NOCTTY);
        if (fd >= 0) {
            struct termios t;
            /* Only accept it if TCGETS actually works — Akuma returns ENOTTY
             * for a channel that is a pipe, which is what an ssh session
             * without a pty gets. */
            if (syscall(SYS_ioctl, fd, 0x5401, &t) == 0) return fd;
            close(fd);
        }
    }
    return -1;
}

static void probe_termios(void)
{
    printf("termios (TCGETS/TCSETS cc[] copy)\n");
    int fd = open_tty();
    if (fd < 0) { skip("termios round-trip", "no tty available on this fd set"); return; }

    struct termios t, back;
    if (syscall(SYS_ioctl, fd, 0x5401 /*TCGETS*/, &t) != 0) {
        skip("termios round-trip", "TCGETS failed"); close(fd); return;
    }
    unsigned char *cc = (unsigned char *)t.c_cc;
    unsigned char saved[20];
    memcpy(saved, cc, 20);
    if (verbose) {
        printf("          cc before =");
        for (int i = 0; i < 20; i++) printf(" %02x", cc[i]);
        printf("\n");
    }
    /* A distinct value in every one of the 20 bytes: an off-by-one in the copy
     * length, or a copy that stops at the first NUL, shows up as a mismatch at
     * a known index rather than as "some bytes differ". */
    for (int i = 0; i < 20; i++) cc[i] = (unsigned char)(0x40 + i);
    ck(syscall(SYS_ioctl, fd, 0x5402 /*TCSETS*/, &t) == 0, "TCSETS accepted the cc[] we wrote");

    memset(&back, 0, sizeof back);
    ck(syscall(SYS_ioctl, fd, 0x5401, &back) == 0, "TCGETS after TCSETS");
    unsigned char *bc = (unsigned char *)back.c_cc;
    int bad = -1;
    for (int i = 0; i < 20; i++) if (bc[i] != (unsigned char)(0x40 + i)) { bad = i; break; }
    if (bad >= 0)
        printf("          first mismatch at cc[%d]: wrote %02x read %02x\n",
               bad, 0x40 + bad, bc[bad]);
    ck(bad < 0, "all 20 cc[] bytes round-tripped (cc[19] is the one an off-by-one drops)");
    ck(back.c_iflag == t.c_iflag && back.c_oflag == t.c_oflag &&
       back.c_cflag == t.c_cflag && back.c_lflag == t.c_lflag,
       "the four flag words round-tripped alongside cc[]");

    memcpy(cc, saved, 20);
    syscall(SYS_ioctl, fd, 0x5402, &t);
    close(fd);
}

/* ---------------------------------------------------------------- eventfd */
static void probe_eventfd(void)
{
    printf("eventfd (u64 decode out of the write buffer)\n");
#ifdef SYS_eventfd2
    int fd = (int)syscall(SYS_eventfd2, 0, 0);
#else
    int fd = -1; errno = ENOSYS;
#endif
    if (fd < 0) { skip("eventfd round-trip", strerror(errno)); return; }

    /* Every octet distinct and non-zero: catches truncation to 32 bits, a
     * byte-swap, and a read that stopped at the first zero byte. */
    unsigned long long v = 0x8899AABBCCDDEEFFULL, back = 0;
    ck(write(fd, &v, 8) == 8, "write(eventfd, 8) accepted");
    ck(read(fd, &back, 8) == 8, "read(eventfd, 8) returned 8");
    if (back != v) printf("          wrote %016llx  read %016llx\n", v, back);
    ck(back == v, "the counter survived the u64 decode intact");

    unsigned long long ones = ~0ULL;
    ck(write(fd, &ones, 8) < 0 && errno == EINVAL, "write(~0) rejected with EINVAL");
    close(fd);
}

/* --------------------------------------------------------------- aio ring */
struct aio_ring_hdr {
    unsigned int id, nr, head, tail, magic, compat_features, incompat_features, header_length;
};

static void probe_aio_ring(void)
{
    printf("aio_ring (io_setup's mapped struct aio_ring header)\n");
    unsigned long ctx = 0;
    const unsigned nr_req = 64;
    if (syscall(SYS_io_setup, nr_req, &ctx) != 0) {
        skip("io_setup ring header", strerror(errno)); return;
    }
    ck(ctx > 0x1000, "io_setup wrote a real mapped ring VA, not a small handle");
    if (ctx <= 0x1000) return;   /* dereferencing it would fault */

    const struct aio_ring_hdr *h = (const struct aio_ring_hdr *)ctx;
    printf("          id=%u nr=%u head=%u tail=%u magic=%08x compat=%u incompat=%u hdrlen=%u\n",
           h->id, h->nr, h->head, h->tail, h->magic,
           h->compat_features, h->incompat_features, h->header_length);
    ck(h->magic == 0xa10a10a1u, "magic == AIO_RING_MAGIC (what glibc checks first)");
    ck(h->header_length == sizeof(struct aio_ring_hdr), "header_length == sizeof(struct aio_ring)");
    ck(h->head == 0 && h->tail == 0, "head/tail start empty");
    ck(h->id == 0 && h->compat_features == 0 && h->incompat_features == 0,
       "id and both feature words are zero");
    if (h->nr != nr_req)
        printf("          DIVERGE nr=%u for a requested %u (Akuma caps the ring at one page)\n",
               h->nr, nr_req);
    ck(h->nr > 0 && h->nr <= nr_req, "nr is non-zero and never above what was asked for");
    syscall(SYS_io_destroy, ctx);
}

/* -------------------------------------------------------------- affinity */
static void probe_affinity(void)
{
    printf("sched_getaffinity (mask write into a Vec<u8>-backed buffer)\n");
    unsigned char mask[128];
    memset(mask, 0xAA, sizeof mask);
    long r = syscall(SYS_sched_getaffinity, 0, sizeof mask, mask);
    if (r < 0) { skip("sched_getaffinity", strerror(errno)); return; }
    printf("          ret=%ld  mask[0..7] = %02x %02x %02x %02x %02x %02x %02x %02x\n",
           r, mask[0], mask[1], mask[2], mask[3], mask[4], mask[5], mask[6], mask[7]);
    ck(r > 0, "returns the number of bytes written, not 0 (musl memsets the rest from this)");
    ck(mask[0] != 0xAA, "byte 0 was actually written over the poison");
    ck((mask[0] & 1) != 0, "cpu 0 is in the mask");
    int n = 0;
    for (long i = 0; i < r && i < (long)sizeof mask; i++)
        for (int b = 0; b < 8; b++) if (mask[i] & (1 << b)) n++;
    printf("          %d cpu%s set in the first %ld byte%s\n", n, n == 1 ? "" : "s", r, r == 1 ? "" : "s");
    ck(n >= 1, "at least one cpu is online");
    ck(n == (int)sysconf(_SC_NPROCESSORS_ONLN),
       "mask popcount agrees with _SC_NPROCESSORS_ONLN (what nproc and cargo -j read)");
}

/* ------------------------------------------------------------- getdents64 */
struct ldirent64 {
    unsigned long long d_ino;
    long long          d_off;
    unsigned short     d_reclen;
    unsigned char      d_type;
    char               d_name[];
};

static void probe_getdents(const char *dir)
{
    printf("getdents64 (raw linux_dirent64 record stream from %s)\n", dir);
    int fd = open(dir, O_RDONLY | O_DIRECTORY);
    if (fd < 0) { skip("getdents64", strerror(errno)); return; }

    static char buf[64 * 1024];
    long n = syscall(SYS_getdents64, fd, buf, sizeof buf);
    if (n < 0) { skip("getdents64", strerror(errno)); close(fd); return; }

    long off = 0;
    int entries = 0, misaligned = 0, overlong = 0, unterminated = 0, dirty_pad = 0, empty = 0;
    while (off < n) {
        if (off + (long)sizeof(struct ldirent64) > n) { overlong++; break; }
        const struct ldirent64 *d = (const struct ldirent64 *)(buf + off);
        unsigned rl = d->d_reclen;
        if (rl == 0 || off + (long)rl > n) { overlong++; break; }
        if (rl % 8) misaligned++;
        /* The name must be NUL-terminated strictly inside the record. */
        size_t maxname = rl - offsetof(struct ldirent64, d_name);
        size_t len = strnlen(d->d_name, maxname);
        if (len == maxname) unterminated++;
        else {
            if (len == 0) empty++;
            /* Everything from the NUL to the end of the record is padding and
             * must be zero — the safe rewrite relies on the record buffer being
             * zero-filled rather than writing the pad explicitly. */
            for (size_t i = offsetof(struct ldirent64, d_name) + len; i < rl; i++)
                if (buf[off + i] != 0) { dirty_pad++; break; }
        }
        if (verbose && entries < 4)
            printf("          [%d] ino=%llu off=%lld reclen=%u type=%u name=\"%.*s\"\n",
                   entries, d->d_ino, d->d_off, rl, d->d_type, (int)len, d->d_name);
        entries++;
        off += rl;
    }
    printf("          %ld bytes, %d entries\n", n, entries);
    ck(entries > 0, "at least one entry returned");
    ck(off == n, "the reclen chain lands exactly on the returned byte count");
    ck(misaligned == 0, "every d_reclen is 8-byte aligned");
    ck(overlong == 0, "no record claims to run past the buffer");
    ck(unterminated == 0, "every d_name is NUL-terminated inside its record");
    ck(empty == 0, "no zero-length names");
    ck(dirty_pad == 0, "the pad between the name's NUL and d_reclen is zero");
    close(fd);
}

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++)
        if (!strcmp(argv[i], "-v")) verbose = 1;
    setvbuf(stdout, NULL, _IOLBF, 0);

    printf("abi_write_probe: kernel->user struct serialization\n\n");
    probe_termios();      printf("\n");
    probe_eventfd();      printf("\n");
    probe_aio_ring();     printf("\n");
    probe_affinity();     printf("\n");
    probe_getdents("/bin");

    printf("\nRESULT: %d checks, %d failed, %d skipped\n", checks, fails, skips);
    return fails ? 1 : 0;
}
