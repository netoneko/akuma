/*
 * linkprobe: hard links as link(2) promises them (docs/archive/HARD_LINKS.md).
 *
 * Every line says ok or FAIL against what Linux does; the last line counts
 * FAILs. Same static binary on Akuma and on Linux (docker run --rm -v
 * $PWD:/p alpine /p/linkprobe).
 *
 * Akuma before 2026-10-14: linkat copied the file, so the two names had
 * different inodes and st_nlink stayed 1, a write through one name never
 * reached the other, and link() onto an existing name overwrote it instead of
 * failing EEXIST — which the race at the end turns into two winners a round.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define D "/tmp/linkprobe"

static int fails;

static const char *ename(int e) {
    switch (e) {
    case 0: return "0";
    case EEXIST: return "EEXIST";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EXDEV: return "EXDEV";
    case EINVAL: return "EINVAL";
    default: return strerror(e);
    }
}

static void expect(const char *what, long rc, int want) {
    int got = rc < 0 ? errno : 0;
    int ok = got == want;
    printf("linkprobe %-44s want %-6s got %-6s %s\n", what, ename(want), ename(got), ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

static void expect_eq(const char *what, long got, long want) {
    int ok = got == want;
    printf("linkprobe %-44s want %-6ld got %-6ld %s\n", what, want, got, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

static void put(const char *p, const char *s) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) { (void)write(fd, s, strlen(s)); close(fd); }
}

static long nlink_of(const char *p) {
    struct stat st;
    return lstat(p, &st) == 0 ? (long)st.st_nlink : -1;
}

static long ino_of(const char *p) {
    struct stat st;
    return lstat(p, &st) == 0 ? (long)st.st_ino : -1;
}

static int first_byte(const char *p) {
    char c = 0;
    int fd = open(p, O_RDONLY);
    if (fd < 0) return -1;
    (void)read(fd, &c, 1);
    close(fd);
    return c;
}

/* Two processes race link(tmp_i, lock) for `rounds`; exactly one may win each. */
static void race(int rounds) {
    int double_wins = 0, no_wins = 0;
    for (int r = 0; r < rounds; r++) {
        unlink(D "/lock");
        put(D "/t0", "0");
        put(D "/t1", "1");
        int pfd[2];
        if (pipe(pfd) != 0) { perror("pipe"); fails++; return; }
        pid_t kids[2];
        for (int k = 0; k < 2; k++) {
            kids[k] = fork();
            if (kids[k] == 0) {
                char go;
                close(pfd[1]);
                (void)read(pfd[0], &go, 1); /* both start together */
                _exit(link(k ? D "/t1" : D "/t0", D "/lock") == 0 ? 1 : 0);
            }
        }
        close(pfd[0]);
        (void)write(pfd[1], "gg", 2);
        close(pfd[1]);
        int wins = 0;
        for (int k = 0; k < 2; k++) {
            int st = 0;
            waitpid(kids[k], &st, 0);
            if (WIFEXITED(st) && WEXITSTATUS(st) == 1) wins++;
        }
        if (wins > 1) double_wins++;
        if (wins == 0) no_wins++;
    }
    expect_eq("link-as-lock race: rounds with 2 winners", double_wins, 0);
    expect_eq("link-as-lock race: rounds with 0 winners", no_wins, 0);
}

int main(void) {
    (void)system("rm -rf " D);
    mkdir(D, 0755);
    put(D "/a", "abc");

    expect("link(a, b)", link(D "/a", D "/b"), 0);
    expect_eq("same inode", ino_of(D "/a") == ino_of(D "/b"), 1);
    expect_eq("st_nlink of a", nlink_of(D "/a"), 2);
    int fd = open(D "/b", O_WRONLY);
    (void)write(fd, "X", 1);
    close(fd);
    expect_eq("write via b, read via a", first_byte(D "/a"), 'X');

    put(D "/taken", "keep");
    expect("link onto an existing name", link(D "/a", D "/taken"), EEXIST);
    expect_eq("existing name untouched", first_byte(D "/taken"), 'k');

    mkdir(D "/dir", 0755);
    expect("link a directory", link(D "/dir", D "/dir2"), EPERM);
    expect("link a missing source", link(D "/missing", D "/m"), ENOENT);
    expect("link into a missing directory", link(D "/a", D "/nodir/x"), ENOENT);
    expect("linkat with an unknown flag", linkat(AT_FDCWD, D "/a", AT_FDCWD, D "/q", 0x8), EINVAL);

    /* A link to a symlink names the symlink unless AT_SYMLINK_FOLLOW. */
    symlink("a", D "/sl");
    expect("link(symlink) names the symlink", link(D "/sl", D "/sl2"), 0);
    struct stat st;
    lstat(D "/sl2", &st);
    expect_eq("  ...and is a symlink", S_ISLNK(st.st_mode), 1);
    expect("linkat(AT_SYMLINK_FOLLOW)", linkat(AT_FDCWD, D "/sl", AT_FDCWD, D "/fl", AT_SYMLINK_FOLLOW), 0);
    expect_eq("  ...names the target's inode", ino_of(D "/fl") == ino_of(D "/a"), 1);
    expect_eq("st_nlink is now 3", nlink_of(D "/a"), 3);

    expect("unlink a", unlink(D "/a"), 0);
    expect_eq("b still reads", first_byte(D "/b"), 'X');
    expect_eq("st_nlink of b", nlink_of(D "/b"), 2);

    /* rename(x, y) where both already name one inode is a no-op success. */
    expect("rename between two links of one file", rename(D "/b", D "/fl"), 0);
    expect_eq("  ...both names still there", ino_of(D "/b") >= 0 && ino_of(D "/fl") >= 0, 1);

    struct stat d;
    stat(D, &d);
    expect_eq("directory st_nlink (2 + one subdir)", d.st_nlink, 3);

    race(200);

    (void)system("rm -rf " D);
    printf("linkprobe: %d fail\n", fails);
    return fails ? 1 : 0;
}
