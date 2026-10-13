/*
 * renameprobe: rename(2) on directories, as Linux does it
 * (docs/archive/BOX_BUILD.md §5 step 3).
 *
 * Usage: renameprobe [dir]   (default /tmp/renameprobe; it is created and removed)
 *
 * Same static binary on Akuma and Linux. Akuma before 2026-10-14: a directory
 * moved to a new parent kept `..` pointing at the old one with both parents'
 * st_nlink stale, a rename onto a non-empty directory "succeeded" and orphaned
 * its contents, and inside a container every directory rename was EIO.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int fails;
static char base[256];

static const char *ename(int e) {
    switch (e) {
    case 0: return "0";
    case ENOTEMPTY: return "ENOTEMPTY";
    case ENOTDIR: return "ENOTDIR";
    case EISDIR: return "EISDIR";
    case EINVAL: return "EINVAL";
    case EXDEV: return "EXDEV";
    case EEXIST: return "EEXIST";
    default: return strerror(e);
    }
}

static const char *P(const char *rel) {
    static char bufs[4][512];
    static int i;
    i = (i + 1) % 4;
    snprintf(bufs[i], sizeof bufs[i], "%s/%s", base, rel);
    return bufs[i];
}

static void expect(const char *what, long rc, int want) {
    int got = rc < 0 ? errno : 0;
    int ok = got == want;
    printf("renameprobe %-46s want %-9s got %-9s %s\n", what, ename(want), ename(got), ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

static void expect_eq(const char *what, long got, long want) {
    int ok = got == want;
    printf("renameprobe %-46s want %-9ld got %-9ld %s\n", what, want, got, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

static void put(const char *p) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) { (void)write(fd, "x", 1); close(fd); }
}

static long ino(const char *p) {
    struct stat st;
    return stat(p, &st) == 0 ? (long)st.st_ino : -1;
}

static long nlink(const char *p) {
    struct stat st;
    return stat(p, &st) == 0 ? (long)st.st_nlink : -1;
}

int main(int argc, char **argv) {
    snprintf(base, sizeof base, "%s", argc > 1 ? argv[1] : "/tmp/renameprobe");
    char cmd[300];
    snprintf(cmd, sizeof cmd, "rm -rf %s", base);
    (void)system(cmd);
    mkdir(base, 0755);
    mkdir(P("a"), 0755);
    mkdir(P("b"), 0755);
    mkdir(P("a/d"), 0755);
    put(P("a/d/f"));
    mkdir(P("full"), 0755);
    put(P("full/g"));
    mkdir(P("empty"), 0755);
    put(P("file"));

    expect("rename(dir, dir/sub)", rename(P("a"), P("a/d/x")), EINVAL);
    expect("rename(dir, non-empty dir)", rename(P("a/d"), P("full")), ENOTEMPTY);
    expect_eq("  ...destination contents intact", access(P("full/g"), F_OK), 0);
    expect("rename(dir, file)", rename(P("a/d"), P("file")), ENOTDIR);
    expect("rename(file, dir)", rename(P("file"), P("empty")), EISDIR);

    expect("rename(dir, other parent)", rename(P("a/d"), P("b/d")), 0);
    expect_eq("  ...`..` is the new parent", ino(P("b/d/..")) == ino(P("b")), 1);
    expect_eq("  ...old parent st_nlink", nlink(P("a")), 2);
    expect_eq("  ...new parent st_nlink", nlink(P("b")), 3);
    expect_eq("  ...contents moved", access(P("b/d/f"), F_OK), 0);

    long base_links = nlink(base);
    expect("rename(dir, empty dir) replaces it", rename(P("b/d"), P("empty")), 0);
    expect_eq("  ...contents under the new name", access(P("empty/f"), F_OK), 0);
    expect_eq("  ...base st_nlink unchanged (one out, one in)", nlink(base), base_links);
    expect_eq("  ...b st_nlink back to 2", nlink(P("b")), 2);

    (void)system(cmd);
    printf("renameprobe: %d fail\n", fails);
    return fails ? 1 : 0;
}
