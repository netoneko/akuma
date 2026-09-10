/*
 * lazybuf.c — every syscall that writes into user memory, aimed at a page the
 * process has **never touched**.
 *
 * # What this exists to catch
 *
 * A kernel can serve a user buffer two ways: copy into it and let the page
 * fault be recovered, or walk the page table first and refuse a range that is
 * not present. The second only works if "not present" can be turned into
 * "present" — i.e. if demand paging is reachable from the syscall path and not
 * only from the `#PF` handler.
 *
 * amd64 C1 step 4b batch 3a folded `read`/`pread64`/`getdents64` into
 * `akuma-syscalls-glue`, whose arms ask first. The prefault hook behind that
 * question was unregistered on that target, fail-closed, so **every one of
 * these calls answered `EFAULT` for a freshly `mmap`ed destination** — the
 * commonest destination there is. What surfaced was `apk update` printing
 * `Unable to read database: v2 database format error`: a file-format complaint
 * about a file the kernel had refused to read.
 *
 * Neither existing gate could see it. The boot self-tests run under
 * `BypassValidationGuard`, which returns before the walk; `amd64_ring3_check.py`
 * reads into libc heap buffers, which are already resident because the
 * allocator wrote a header into them. **A page nothing has touched is the whole
 * probe** — hence the `TOUCHED` control below, which does the identical call
 * into the identical buffer after one store, and must pass either way.
 *
 * Each probe prints PASS / FAIL / SKIP in the house shape. Run the same static
 * binary on Linux: every line should be PASS there.
 *
 * Static, musl, no Rust runtime. Build:
 *   x86_64-linux-musl-gcc -O2 -static -o lazybuf lazybuf.c
 *   aarch64-linux-musl-gcc -O2 -static -o lazybuf lazybuf.c
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int passed, failed, skipped;

static void ok(const char *what) { passed++; printf("PASS %s\n", what); }
static void no(const char *what, const char *why)
{
    failed++;
    printf("FAIL %s — %s\n", what, why);
}
static void skip(const char *what, const char *why)
{
    skipped++;
    printf("SKIP %s — %s\n", what, why);
}

/*
 * **1 MiB, and the size is the probe.** Akuma populates a small `mmap`
 * eagerly — `akuma_config::MMAP_EAGER_MAX_PAGES` is 16 pages, i.e. 64 KiB — so
 * a mapping at or below that threshold is resident the moment `mmap` returns
 * and every probe below passes on a kernel with no demand paging reachable
 * from the syscall path at all. The first draft of this file used exactly
 * 64 KiB and did precisely that: 6/6 PASS against the kernel it was written to
 * fail on, while `apk` was still broken three feet away. Stay well clear of
 * the threshold, and read a *small* amount out of a *large* mapping.
 */
#define BUF_BYTES (1024 * 1024)

/*
 * A writable anonymous mapping whose pages have never been written to.
 *
 * `MAP_ANONYMOUS` promises zeros, which a kernel may deliver eagerly or
 * lazily; this probe is about the lazy case and there is no portable way to
 * *demand* it. What it can do is stay far above the size at which this kernel
 * chooses lazily — see [`BUF_BYTES`] — so that "the page is not there yet" is
 * the actual state under test rather than a hope. On Linux, which is lazy at
 * every size, that is automatic.
 */
static void *fresh(void)
{
    void *p = mmap(NULL, BUF_BYTES, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? NULL : p;
}

/* The file every read probe below reads. Something that always exists and is
 * bigger than a page, so a 4 KiB read is a real read rather than an EOF. */
static const char *big_file(void)
{
    static const char *candidates[] = {
        "/bin/busybox", "/bin/sh", "/proc/self/exe", NULL,
    };
    for (int i = 0; candidates[i]; i++) {
        struct stat st;
        if (stat(candidates[i], &st) == 0 && st.st_size > 8192)
            return candidates[i];
    }
    return NULL;
}

static void probe_read(const char *path, int touch)
{
    const char *what = touch ? "read(2) into a TOUCHED mmap page (control)"
                             : "read(2) into an untouched mmap page";
    void *buf = fresh();
    if (!buf) { skip(what, "mmap failed"); return; }
    if (touch)
        memset(buf, 0, BUF_BYTES);
    int fd = open(path, O_RDONLY);
    if (fd < 0) { skip(what, strerror(errno)); munmap(buf, BUF_BYTES); return; }
    ssize_t n = read(fd, buf, 4096);
    if (n < 0)
        no(what, strerror(errno));
    else if (n == 0)
        no(what, "read returned 0 on a file with bytes in it");
    else
        ok(what);
    close(fd);
    munmap(buf, BUF_BYTES);
}

static void probe_pread(const char *path)
{
    const char *what = "pread64(2) into an untouched mmap page";
    void *buf = fresh();
    if (!buf) { skip(what, "mmap failed"); return; }
    int fd = open(path, O_RDONLY);
    if (fd < 0) { skip(what, strerror(errno)); munmap(buf, BUF_BYTES); return; }
    ssize_t n = pread(fd, buf, 4096, 4096);
    if (n < 0)
        no(what, strerror(errno));
    else if (n == 0)
        no(what, "pread returned 0 inside a file bigger than 8 KiB");
    else
        ok(what);
    close(fd);
    munmap(buf, BUF_BYTES);
}

static void probe_getdents(void)
{
    const char *what = "getdents64(2) into an untouched mmap page";
    void *buf = fresh();
    if (!buf) { skip(what, "mmap failed"); return; }
    int fd = open("/", O_RDONLY | O_DIRECTORY);
    if (fd < 0) { skip(what, strerror(errno)); munmap(buf, BUF_BYTES); return; }
    long n = syscall(SYS_getdents64, fd, buf, 4096);
    if (n < 0)
        no(what, strerror(errno));
    else if (n == 0)
        no(what, "the root directory listed as empty");
    else
        ok(what);
    close(fd);
    munmap(buf, BUF_BYTES);
}

static void probe_fstat(const char *path)
{
    const char *what = "fstat(2) into an untouched mmap page";
    void *buf = fresh();
    if (!buf) { skip(what, "mmap failed"); return; }
    int fd = open(path, O_RDONLY);
    if (fd < 0) { skip(what, strerror(errno)); munmap(buf, BUF_BYTES); return; }
    /* Through the raw syscall: libc's `fstat` may marshal through its own
     * stack `struct stat` and copy out, which would hide the very thing this
     * asks about.
     *
     * This one **passed against the broken kernel** and that is not a flaw in
     * it: `fstat` is not an `akuma-syscalls-glue` arm on amd64 yet (4b batch 3a
     * folded `read`/`pread64`/`getdents64`/`write`/`lseek` and stopped short of
     * the `stat` family, which needs a `struct stat` layout hop). It is here so
     * that the day `fstat` folds, this line starts asking the question rather
     * than having to be remembered. */
    if (syscall(SYS_fstat, fd, buf) < 0)
        no(what, strerror(errno));
    else if (((struct stat *)buf)->st_size == 0)
        no(what, "st_size came back 0 for a file with bytes in it");
    else
        ok(what);
    close(fd);
    munmap(buf, BUF_BYTES);
}

/*
 * The mirror image: a *source* buffer the kernel reads out of. It has to be
 * written before it can be written *from*, so it is never lazy — which is
 * exactly why `write(2)` did not surface this bug and is worth stating rather
 * than leaving as a gap in the list.
 */
static void probe_write_from_fresh(void)
{
    const char *what = "write(2) from an mmap page (resident by construction)";
    void *buf = fresh();
    if (!buf) { skip(what, "mmap failed"); return; }
    memcpy(buf, "lazybuf\n", 8);
    int fd = open("/tmp/lazybuf.probe", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { skip(what, strerror(errno)); munmap(buf, BUF_BYTES); return; }
    ssize_t n = write(fd, buf, 8);
    if (n == 8)
        ok(what);
    else
        no(what, n < 0 ? strerror(errno) : "short write");
    close(fd);
    unlink("/tmp/lazybuf.probe");
    munmap(buf, BUF_BYTES);
}

int main(void)
{
    const char *path = big_file();
    if (!path) {
        skip("all read probes", "no file larger than 8 KiB to read");
    } else {
        probe_read(path, 0);
        probe_read(path, 1);
        probe_pread(path);
        probe_fstat(path);
    }
    probe_getdents();
    probe_write_from_fresh();

    printf("lazybuf: %d passed, %d FAILED, %d skipped\n",
           passed, failed, skipped);
    return failed ? 1 : 0;
}
