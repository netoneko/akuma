/*
 * openflags.c — probe `open(2)`'s flag vocabulary against Linux semantics, bit
 * by bit.
 *
 * Written as the correctness gate for amd64 C1 step 4b batch 2d
 * (docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2D.md), which folded this target's
 * `openat` body into `akuma-syscalls-glue` and left four refusals behind as a
 * preamble — `O_DIRECTORY`, `O_EXCL`, `O_NOFOLLOW` and `O_CREAT`-on-a-directory.
 * Three of those four exist because a real program tripped over their absence
 * (apk's `O_TMPFILE` probe, twice), and until this file existed the only thing
 * asserting them was a kernel-side self-test running under
 * `BypassValidationGuard` — which cannot see what ring 3 sees.
 *
 * Each probe prints PASS (matches Linux) / FAIL (diverges) / SKIP (the
 * environment could not run it) / DIVERGE (a *known*, documented difference
 * from Linux; not counted as a failure).
 *
 * Run the same static binary on Linux to confirm the probes themselves are
 * right: every FAIL here should be a PASS there, and every DIVERGE here should
 * be a PASS there.
 *
 * Static, musl, no Rust runtime. Build:
 *   x86_64-linux-musl-gcc -O2 -static -o openflags openflags.c
 *   aarch64-linux-musl-gcc -O2 -static -o openflags openflags.c
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int passed, failed, skipped, diverged;

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
static void diverge(const char *what, const char *why)
{
    diverged++;
    printf("DIVERGE %s — %s\n", what, why);
}

/* `open(path, flags, mode)` must fail with exactly `want`. */
static void expect_errno(const char *what, const char *path, int flags,
                         mode_t mode, int want)
{
    int fd = open(path, flags, mode);
    if (fd >= 0) {
        close(fd);
        no(what, "the open succeeded");
        return;
    }
    if (errno != want) {
        char buf[96];
        snprintf(buf, sizeof buf, "errno %d (%s), wanted %d", errno,
                 strerror(errno), want);
        no(what, buf);
        return;
    }
    ok(what);
}

static void expect_ok(const char *what, const char *path, int flags, mode_t mode)
{
    int fd = open(path, flags, mode);
    if (fd < 0) {
        char buf[96];
        snprintf(buf, sizeof buf, "errno %d (%s)", errno, strerror(errno));
        no(what, buf);
        return;
    }
    close(fd);
    ok(what);
}

static const char *DIR = "/tmp";
static char reg[128], missing[128], link_[128], dangling[128], excl[128],
    made[128];

static void paths(void)
{
    /* `/tmp` where there is one, the root where there is not: this target has
     * no per-process cwd and its images carry no `/tmp` on every rootfs. */
    struct stat st;
    if (stat("/tmp", &st) != 0 || !S_ISDIR(st.st_mode))
        DIR = "";
    snprintf(reg, sizeof reg, "%s/openflags_reg", DIR);
    snprintf(missing, sizeof missing, "%s/openflags_absent", DIR);
    snprintf(link_, sizeof link_, "%s/openflags_link", DIR);
    snprintf(dangling, sizeof dangling, "%s/openflags_dangling", DIR);
    snprintf(excl, sizeof excl, "%s/openflags_excl", DIR);
    snprintf(made, sizeof made, "%s/openflags_made", DIR);
}

int main(void)
{
    paths();
    printf("openflags: probing open(2) flags in %s\n", DIR[0] ? DIR : "/");

    unlink(reg); unlink(link_); unlink(dangling); unlink(excl); unlink(made);

    /* A regular file to ask the questions of. */
    int fd = open(reg, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        printf("openflags: cannot create %s (%s) — nothing to probe\n", reg,
               strerror(errno));
        return 2;
    }
    if (write(fd, "abc", 3) != 3)
        printf("openflags: short write to %s\n", reg);
    close(fd);

    /* ---- O_EXCL ---------------------------------------------------------
     * The whole contract of the flag: it is how a caller claims a lock file or
     * an atomic temp name, and succeeding anyway tells two of them they both
     * won. */
    expect_errno("O_CREAT|O_EXCL on an existing file is EEXIST", reg,
                 O_WRONLY | O_CREAT | O_EXCL, 0644, EEXIST);
    expect_ok("O_CREAT|O_EXCL on a new name succeeds", excl,
              O_WRONLY | O_CREAT | O_EXCL, 0644);
    expect_errno("and the same name a second time is EEXIST", excl,
                 O_WRONLY | O_CREAT | O_EXCL, 0644, EEXIST);

    /* ---- O_DIRECTORY ----------------------------------------------------
     * The bit ring 3 sets is 0200000 on x86_64 and 040000 on aarch64 — the
     * permutation `akuma_syscalls_abi::open_flags` exists for. Read with the
     * wrong table this tests O_DIRECT and refuses nothing. */
    expect_errno("O_DIRECTORY on a regular file is ENOTDIR", reg,
                 O_RDONLY | O_DIRECTORY, 0, ENOTDIR);
    expect_ok("O_DIRECTORY on a directory succeeds", DIR[0] ? DIR : "/",
              O_RDONLY | O_DIRECTORY, 0);
    /* "There is no such file" outranks "and it would not have been a
     * directory": placed above the existence question this answers ENOTDIR and
     * sends a caller looking for a directory it never asked about. */
    expect_errno("O_DIRECTORY on a missing path is ENOENT, not ENOTDIR",
                 missing, O_RDONLY | O_DIRECTORY, 0, ENOENT);

    /* ---- a directory is not writable ------------------------------------ */
    expect_errno("a write open of a directory is EISDIR", DIR[0] ? DIR : "/",
                 O_WRONLY, 0, EISDIR);
    expect_errno("O_CREAT on an existing directory is EISDIR",
                 DIR[0] ? DIR : "/", O_WRONLY | O_CREAT, 0644, EISDIR);

    /* ---- O_NOFOLLOW ------------------------------------------------------ */
    if (symlink(reg, link_) != 0) {
        skip("O_NOFOLLOW on a symlink is ELOOP", "symlink() failed");
        skip("O_NOFOLLOW on a regular file succeeds", "symlink() failed");
    } else {
        expect_errno("O_NOFOLLOW on a symlink is ELOOP", link_,
                     O_RDONLY | O_NOFOLLOW, 0, ELOOP);
        expect_ok("without the flag the same link opens its target", link_,
                  O_RDONLY, 0);
        expect_ok("O_NOFOLLOW on a regular file succeeds", reg,
                  O_RDONLY | O_NOFOLLOW, 0);
    }

    /* ---- O_TMPFILE -------------------------------------------------------
     * Not implemented, and Linux kernels that predate it answer EINVAL —
     * portable callers (apk-tools 3) treat any failure as "no tmpfiles here"
     * and fall back to a named `.tmp` + rename. A *real* Linux supports it, so
     * a success there is the expected answer and not a probe failure. */
#ifdef O_TMPFILE
    {
        int t = open(DIR[0] ? DIR : "/", O_RDWR | O_TMPFILE, 0644);
        if (t >= 0) {
            close(t);
            diverge("O_TMPFILE is EINVAL",
                    "this kernel supports it (a real Linux does)");
        } else if (errno == EINVAL || errno == EOPNOTSUPP || errno == ENOTSUP) {
            ok("O_TMPFILE is refused rather than silently ignored");
        } else {
            char buf[96];
            snprintf(buf, sizeof buf, "errno %d (%s)", errno, strerror(errno));
            no("O_TMPFILE is refused rather than silently ignored", buf);
        }
    }
#endif

    /* ---- the ordinary answers -------------------------------------------- */
    expect_errno("a missing path without O_CREAT is ENOENT", missing, O_RDONLY,
                 0, ENOENT);
    {
        char deep[192];
        snprintf(deep, sizeof deep, "%s/openflags_no_such_dir/f", DIR);
        expect_errno("O_CREAT under a missing directory is ENOENT", deep,
                     O_WRONLY | O_CREAT, 0644, ENOENT);
    }

    /* ---- O_TRUNC --------------------------------------------------------- */
    {
        int t = open(reg, O_WRONLY | O_TRUNC, 0);
        struct stat st;
        if (t < 0)
            no("O_TRUNC empties an existing file", strerror(errno));
        else {
            close(t);
            if (stat(reg, &st) == 0 && st.st_size == 0)
                ok("O_TRUNC empties an existing file");
            else
                no("O_TRUNC empties an existing file", "size is not 0");
        }
    }

    /* ---- mode ------------------------------------------------------------
     * The argument this target ignored until the fold: every file it created
     * came out with whatever the filesystem picked, so a `tcc`-built binary
     * needed a `chmod +x` that its build never ran. */
    {
        struct stat st;
        int m = open(made, O_WRONLY | O_CREAT | O_EXCL, 0700);
        if (m < 0)
            skip("O_CREAT honours the mode argument", strerror(errno));
        else {
            close(m);
            if (stat(made, &st) != 0)
                no("O_CREAT honours the mode argument", "stat failed");
            else if ((st.st_mode & 0777) == 0700)
                ok("O_CREAT honours the mode argument");
            else {
                char buf[96];
                snprintf(buf, sizeof buf, "mode is 0%o, wanted 0700",
                         st.st_mode & 0777);
                no("O_CREAT honours the mode argument", buf);
            }
        }
    }

    /* ---- openat against a directory descriptor ---------------------------
     * `apk` loads every signing key with `openat(keys_dirfd, name)` after
     * listing that directory; while `dirfd` was ignored each such open landed
     * on a root-relative name that does not exist, and every fetched index
     * reported UNTRUSTED signature. */
    {
        int d = open(DIR[0] ? DIR : "/", O_RDONLY | O_DIRECTORY);
        if (d < 0)
            skip("openat resolves against a directory fd", strerror(errno));
        else {
            const char *base = strrchr(reg, '/');
            int f = openat(d, base ? base + 1 : reg, O_RDONLY);
            if (f < 0)
                no("openat resolves against a directory fd", strerror(errno));
            else {
                close(f);
                ok("openat resolves against a directory fd");
            }
            /* A bogus negative dirfd must fail rather than resolve against
             * the root — that turned openat(-5, "rel") into a successful open
             * of a *different file*. */
            int b = openat(-5, base ? base + 1 : reg, O_RDONLY);
            if (b >= 0) {
                close(b);
                no("a bogus negative dirfd is EBADF", "the open succeeded");
            } else if (errno == EBADF)
                ok("a bogus negative dirfd is EBADF");
            else
                no("a bogus negative dirfd is EBADF", strerror(errno));
            close(d);
        }
    }

    /* ---- /dev ------------------------------------------------------------ */
    {
        int n = open("/dev/null", O_WRONLY | O_CREAT | O_TRUNC, 0666);
        if (n < 0)
            no("/dev/null opens for a shell's `>`", strerror(errno));
        else {
            if (write(n, "x", 1) == 1)
                ok("/dev/null opens for a shell's `>`");
            else
                no("/dev/null opens for a shell's `>`", "the write failed");
            close(n);
        }
        int z = open("/dev/zero", O_RDONLY);
        if (z < 0)
            no("/dev/zero reads zeros", strerror(errno));
        else {
            char b[8];
            memset(b, 0xff, sizeof b);
            ssize_t got = read(z, b, sizeof b);
            int all0 = 1;
            for (size_t i = 0; i < sizeof b; i++)
                if (b[i]) all0 = 0;
            if (got == (ssize_t)sizeof b && all0)
                ok("/dev/zero reads zeros");
            else
                no("/dev/zero reads zeros", "short read or non-zero bytes");
            close(z);
        }
    }

    unlink(reg); unlink(link_); unlink(dangling); unlink(excl); unlink(made);

    printf("openflags: %d passed, %d FAILED, %d skipped, %d known divergences\n",
           passed, failed, skipped, diverged);
    return failed ? 1 : 0;
}
