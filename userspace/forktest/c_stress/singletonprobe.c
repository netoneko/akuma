/*
 * singletonprobe: Chromium's ProcessSingleton setup, step for step as Linux
 * strace shows it (chrome/browser/process_singleton_posix.cc), each step
 * reported with its errno. On Akuma Chromium logs "Failed to create socket
 * directory", which is the mkdtemp. Linux: every step 0.
 *   singletonprobe [profile-dir]   default /tmp/singleton-prof
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static int fails;
static void step(const char *what, int rc) {
    printf("singletonprobe: %-44s = %d%s%s\n", what, rc, rc < 0 ? " " : "", rc < 0 ? strerror(errno) : "");
    fflush(stdout);
    if (rc < 0) fails++;
}

int main(int argc, char **argv) {
    const char *prof = argc > 1 ? argv[1] : "/tmp/singleton-prof";
    char a[512], b[512], buf[256];
    mkdir(prof, 0700);
    snprintf(a, sizeof a, "%s/SingletonLock", prof);
    printf("singletonprobe: readlink lock (expect -1 ENOENT) = %zd\n", readlink(a, buf, sizeof buf));
    step("symlink(host-pid, SingletonLock)", symlink("akuma-1", a));

    char tmpl[] = "/tmp/.org.chromium.Chromium.scoped_dir.XXXXXX";
    char *dir = mkdtemp(tmpl);
    step("mkdtemp(/tmp/.org.chromium...XXXXXX)", dir ? 0 : -1);
    if (!dir) {
        /* What mkdtemp does inside: mkdir of a random name. Try it by hand. */
        step("mkdir(/tmp/.org.chromium.Chromium.scoped_dir.abcdef)",
             mkdir("/tmp/.org.chromium.Chromium.scoped_dir.abcdef", 0700));
        printf("singletonprobe: FAIL\n");
        return 1;
    }
    struct stat sb;
    step("stat(dir)", stat(dir, &sb));
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    step("socket(AF_UNIX, SOCK_STREAM)", s);
    snprintf(a, sizeof a, "%s/SingletonSocket", prof);
    unlink(a);
    snprintf(b, sizeof b, "%s/SingletonSocket", dir);
    step("symlink(dir/SingletonSocket, prof/SingletonSocket)", symlink(b, a));
    snprintf(a, sizeof a, "%s/SingletonCookie", prof);
    unlink(a);
    step("symlink(cookie, prof/SingletonCookie)", symlink("1234567890", a));
    snprintf(a, sizeof a, "%s/SingletonCookie", dir);
    step("symlink(cookie, dir/SingletonCookie)", symlink("1234567890", a));
    struct sockaddr_un sun = {.sun_family = AF_UNIX};
    strncpy(sun.sun_path, b, sizeof sun.sun_path - 1);
    step("bind(dir/SingletonSocket)", bind(s, (struct sockaddr *)&sun, sizeof sun));
    step("listen", listen(s, 5));
    snprintf(a, sizeof a, "%s/SingletonSocket", prof);
    ssize_t n = readlink(a, buf, sizeof buf - 1);
    step("readlink(prof/SingletonSocket)", n < 0 ? -1 : 0);
    printf("singletonprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
