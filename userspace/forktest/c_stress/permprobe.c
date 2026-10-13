/*
 * permprobe: Unix credentials and file permissions, as an unprivileged process
 * sees them (docs/archive/UNIX_PERMISSIONS.md).
 *
 * Runs as root. Builds a fixture tree under /tmp/permprobe, then forks a child
 * that drops to uid 1000 / gid 1000 / groups {50} the way gosu and su-exec do
 * (setgroups, setgid, setuid) and checks one operation per line against the
 * errno Linux gives. Every line says ok or FAIL; the last line counts FAILs.
 *
 * The same static binary runs on Akuma and on Linux (docker run --rm -v
 * $PWD:/p alpine /p/permprobe), so a FAIL that appears only on Akuma is a
 * kernel divergence. On Akuma the probe turns enforcement on through /proc/dac
 * for the run and restores the previous value; Linux has no such file and
 * always enforces.
 *
 * Akuma before 2026-10-14: setuid was an accepted no-op, so the child stayed
 * uid 0 and every "must be refused" line FAILed.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define D "/tmp/permprobe"

static int fails;

static const char *ename(int e) {
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case EACCES: return "EACCES";
    case ENOENT: return "ENOENT";
    case EINVAL: return "EINVAL";
    case ESRCH: return "ESRCH";
    default: return strerror(e);
    }
}

/* `rc` is the call's return (< 0 = failed, errno set); `want` 0 = must succeed. */
static void expect(const char *what, long rc, int want) {
    int got = rc < 0 ? errno : 0;
    int ok = got == want;
    printf("permprobe %-44s want %-6s got %-6s %s\n", what, ename(want), ename(got), ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

static void expect_eq(const char *what, long got, long want) {
    int ok = got == want;
    printf("permprobe %-44s want %-6ld got %-6ld %s\n", what, want, got, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
}

static long open_close(const char *p, int flags) {
    int fd = open(p, flags, 0644);
    if (fd >= 0) close(fd);
    return fd;
}

static void mkfile(const char *p, mode_t mode, uid_t u, gid_t g) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd >= 0) { (void)write(fd, "x\n", 2); close(fd); }
    chown(p, u, g);
    chmod(p, mode);
}

static void setup(void) {
    (void)system("rm -rf " D);
    mkdir(D, 0755);
    chmod(D, 0755);
    mkfile(D "/root600", 0600, 0, 0);
    mkfile(D "/mine", 0600, 1000, 1000);
    mkfile(D "/group50", 0640, 0, 50);
    mkfile(D "/noexec", 0755 & ~0111, 0, 0);
    mkfile(D "/script", 0755, 0, 0);
    mkdir(D "/ro", 0755);
    chmod(D "/ro", 0755);
    mkdir(D "/sticky", 0777);
    chmod(D "/sticky", 01777);
    mkfile(D "/sticky/theirs", 0644, 2000, 2000);
    mkfile(D "/sticky/mine", 0644, 1000, 1000);
    mkdir(D "/locked", 0700);
    chmod(D "/locked", 0700);
}

static void child(pid_t parent) {
    gid_t groups[] = {50};
    expect("setgroups({50})", setgroups(1, groups), 0);
    expect("setgid(1000)", setgid(1000), 0);
    expect("setuid(1000)", setuid(1000), 0);

    uid_t r, e, s;
    getresuid(&r, &e, &s);
    expect_eq("getuid", getuid(), 1000);
    expect_eq("geteuid", geteuid(), 1000);
    expect_eq("getresuid saved", s, 1000);
    expect_eq("getgid", getgid(), 1000);
    expect_eq("getgroups count", getgroups(0, NULL), 1);
    expect("setuid(1000) again (synccall re-set)", setuid(1000), 0);
    expect("setuid(0) after the drop", setuid(0), EPERM);
    expect("setgid(0) after the drop", setgid(0), EPERM);
    expect("setgroups({0}) after the drop", setgroups(1, (gid_t[]){0}), EPERM);

    /* /proc/self/status reports the drop. */
    char line[128] = {0};
    FILE *f = fopen("/proc/self/status", "r");
    int saw_uid = 0, caps_empty = 0;
    while (f && fgets(line, sizeof line, f)) {
        if (!strncmp(line, "Uid:\t1000\t1000\t1000\t1000", 24)) saw_uid = 1;
        if (!strncmp(line, "CapEff:\t0000000000000000", 24)) caps_empty = 1;
    }
    if (f) fclose(f);
    expect_eq("/proc/self/status Uid: 1000 x4", saw_uid, 1);
    expect_eq("/proc/self/status CapEff empty", caps_empty, 1);

    /* Opening files. */
    expect("open root600 O_RDONLY", open_close(D "/root600", O_RDONLY), EACCES);
    expect("open mine O_RDWR", open_close(D "/mine", O_RDWR), 0);
    expect("open group50 O_RDONLY (supp. group)", open_close(D "/group50", O_RDONLY), 0);
    expect("open group50 O_WRONLY", open_close(D "/group50", O_WRONLY), EACCES);
    expect("open root600 O_RDONLY|O_TRUNC", open_close(D "/root600", O_WRONLY | O_TRUNC), EACCES);
    expect("access(noexec, X_OK)", access(D "/noexec", X_OK), EACCES);
    expect("access(mine, R_OK|W_OK)", access(D "/mine", R_OK | W_OK), 0);
    expect("access(root600, R_OK)", access(D "/root600", R_OK), EACCES);

    /* Creating entries. */
    expect("creat in 0755 root dir", open_close(D "/ro/new", O_WRONLY | O_CREAT), EACCES);
    expect("mkdir in 0755 root dir", mkdir(D "/ro/d", 0755), EACCES);
    expect("symlink in 0755 root dir", symlink("x", D "/ro/l"), EACCES);
    umask(027);
    expect("creat in sticky 1777 dir", open_close(D "/sticky/new", O_WRONLY | O_CREAT), 0);
    struct stat st = {0};
    stat(D "/sticky/new", &st);
    expect_eq("new file uid", st.st_uid, 1000);
    expect_eq("new file gid", st.st_gid, 1000);
    expect_eq("new file mode (0644 & ~027)", st.st_mode & 07777, 0640);

    /* Deleting entries. */
    expect("unlink theirs in sticky dir", unlink(D "/sticky/theirs"), EPERM);
    expect("unlink mine in sticky dir", unlink(D "/sticky/mine"), 0);
    expect("unlink root600 (no w on dir)", unlink(D "/root600"), EACCES);
    expect("rename root600 -> sticky", rename(D "/root600", D "/sticky/x"), EACCES);

    /* Attributes. */
    expect("chmod root600", chmod(D "/root600", 0644), EPERM);
    expect("chmod mine", chmod(D "/mine", 0640), 0);
    expect("chown mine to 2000 (give away)", chown(D "/mine", 2000, -1), EPERM);
    expect("chgrp mine to 50 (a group I am in)", chown(D "/mine", -1, 50), 0);
    expect("chgrp mine to 60", chown(D "/mine", -1, 60), EPERM);
    expect("truncate root600", truncate(D "/root600", 0), EACCES);
    expect("utimes(root600, now)", utimensat(AT_FDCWD, D "/root600", NULL, 0), EACCES);
    struct timespec ts[2] = {{1, 0}, {1, 0}};
    expect("utimes(mine, explicit)", utimensat(AT_FDCWD, D "/mine", ts, 0), 0);

    /* Directories. */
    expect("chdir into 0700 root dir", chdir(D "/locked"), EACCES);
    expect("opendir 0700 root dir", open_close(D "/locked", O_RDONLY | O_DIRECTORY), EACCES);

    /* Exec. */
    pid_t p = fork();
    if (p == 0) {
        execl(D "/noexec", "noexec", (char *)NULL);
        _exit(errno == EACCES ? 0 : 1);
    }
    int ws = 0;
    waitpid(p, &ws, 0);
    expect_eq("execve file with no x bit -> EACCES", WIFEXITED(ws) && WEXITSTATUS(ws) == 0, 1);

    /* Signals. */
    expect("kill(root parent, 0)", kill(parent, 0), EPERM);
    expect("kill(self, 0)", kill(getpid(), 0), 0);

    printf("permprobe child: %d fail\n", fails);
    fflush(stdout);
    _exit(fails ? 1 : 0);
}

static int read_dac(void) {
    char c = 0;
    int fd = open("/proc/dac", O_RDONLY);
    if (fd < 0) return -1;
    (void)read(fd, &c, 1);
    close(fd);
    return c == '1';
}

static void write_dac(int on) {
    int fd = open("/proc/dac", O_WRONLY);
    if (fd < 0) return;
    (void)write(fd, on ? "1\n" : "0\n", 2);
    close(fd);
}

int main(void) {
    if (getuid() != 0) {
        printf("permprobe: must start as root\n");
        return 2;
    }
    int was = read_dac();
    if (was >= 0) write_dac(1);
    if (was >= 0) expect_eq("/proc/dac reads 1 after writing it", read_dac(), 1);

    setup();
    struct stat st = {0};
    stat(D "/mine", &st);
    expect_eq("chown recorded: mine uid", st.st_uid, 1000);
    expect_eq("chown recorded: group50 gid", (stat(D "/group50", &st), (long)st.st_gid), 50);

    pid_t parent = getpid();
    pid_t c = fork();
    if (c == 0) child(parent);
    int ws = 0;
    waitpid(c, &ws, 0);
    if (!(WIFEXITED(ws) && WEXITSTATUS(ws) == 0)) fails++;

    /* Root is unaffected by the switch: it still opens a 0000 file. */
    mkfile(D "/zero", 0, 1000, 1000);
    expect("root opens a 0000 file", open_close(D "/zero", O_RDONLY), 0);

    if (was >= 0) write_dac(was);
    (void)system("rm -rf " D);
    printf("permprobe: %d fail\n", fails);
    return fails ? 1 : 0;
}
