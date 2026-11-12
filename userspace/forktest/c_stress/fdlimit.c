// fdlimit — RLIMIT_NOFILE as reported and as enforced, one line each, for diffing.
//
// Every line is `name result`: a value, or `-errno (text)`. The Linux output is
// the oracle for the limit rows (run with `ulimit -n 1024` there, the limit
// Akuma reports). The `vda_*` rows are Akuma's raw block node and have no
// Linux counterpart in a VM without that disk.
//
// Linux 7.0 at a 1024 limit answers: getrlimit 1024/1024; opens stop with
// EMFILE at fd 1023 (1021 more after 0/1/2); dup and F_DUPFD on a full table
// EMFILE; dup2/dup3 to 1024 EBADF; F_DUPFD from 1024 EINVAL; after one close the
// next open gets the freed number. open(NULL) is EFAULT, open("") ENOENT.
//
// `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "fd: the policies are glue's".

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

static void show(const char *name, long r) {
    if (r < 0)
        printf("%-20s -%d (%s)\n", name, errno, strerror(errno));
    else
        printf("%-20s %ld\n", name, r);
}

int main(void) {
    struct rlimit rl;
    if (getrlimit(RLIMIT_NOFILE, &rl) == 0)
        printf("%-20s %llu/%llu\n", "getrlimit", (unsigned long long)rl.rlim_cur,
               (unsigned long long)rl.rlim_max);

    // Names that are not paths: NULL faults, "" names nothing (neither opens the cwd).
    show("open_null", open(NULL, O_RDONLY));
    show("open_empty", open("", O_RDONLY));

    // Out-of-range targets first, while the table has room.
    show("dup2_to_limit", dup2(0, 1024));
    show("dup3_to_limit", dup3(0, 1024, 0));
    show("dupfd_from_limit", fcntl(0, F_DUPFD, 1024));
    show("dup2_below_limit", dup2(0, 1023));
    close(1023);

    // Fill the table.
    int opened = 0, last = -1;
    for (;;) {
        int fd = open("/dev/null", O_RDONLY);
        if (fd < 0)
            break;
        last = fd;
        opened++;
        if (opened > 5000)
            break;
    }
    int err = errno;
    printf("%-20s %d\n", "opened", opened);
    printf("%-20s %d\n", "last_fd", last);
    errno = err;
    show("open_when_full", -1);
    show("dup_when_full", dup(0));
    show("dupfd_when_full", fcntl(0, F_DUPFD, 3));
    close(500);
    show("open_after_close", open("/dev/null", O_RDONLY));
    for (int fd = 3; fd < 1024; fd++)
        close(fd);

    // Akuma's raw block node: readable, and not writable while mounted.
    int b = open("/dev/vda", O_RDONLY);
    show("vda_open_ro", b);
    if (b >= 0) {
        unsigned char sb[1024];
        // `read` after `lseek`: `pread` on a raw block descriptor is still
        // `EBADF` (pinned in the fd slice).
        long n = lseek(b, 1024, SEEK_SET) == 1024 ? read(b, sb, sizeof sb) : -1;
        show("vda_read_sb", n);
        if (n == (long)sizeof sb)
            printf("%-20s 0x%02x%02x\n", "vda_ext2_magic", sb[57], sb[56]);
        close(b);
    }
    show("vda_open_rw", open("/dev/vda", O_RDWR));
    return 0;
}
