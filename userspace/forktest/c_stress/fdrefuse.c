// fdrefuse — the fd-layer refusals Linux makes, one line each, for diffing.
//
// Every line is `name result`: a count, or `-errno (text)`. The output on a
// Linux kernel is the oracle; a kernel passes when its output is identical.
// Calibrate with this static musl binary on real Linux (Lima `fc` here:
// `limactl shell fc sh -c 'cat >/tmp/f && chmod +x /tmp/f && /tmp/f' < fdrefuse`).
// Linux 7.0 answers:
//
//   open:   O_EXCL on a present file EEXIST, O_DIRECTORY on a file ENOTDIR,
//           O_CREAT on a directory EISDIR, O_NOFOLLOW on a symlink ELOOP
//   rw:     write/pwrite on O_RDONLY EBADF, read/pread on O_WRONLY EBADF
//   seek:   pread/pwrite/lseek on a pipe or socket ESPIPE
//   /dev:   lseek on /dev/null, /dev/zero, /dev/urandom is 0 whatever the
//           offset; an unknown whence is EINVAL
//
// `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "Slice 4: fd", step 3.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

static void show(const char *name, long r) {
    if (r < 0)
        printf("%-22s -%d (%s)\n", name, errno, strerror(errno));
    else
        printf("%-22s %ld\n", name, r);
}

int main(void) {
    char d[64], f[96], l[96], x[96], b[4];
    snprintf(d, sizeof d, "/tmp/fdref.%d", getpid());
    snprintf(f, sizeof f, "%s/f", d);
    snprintf(l, sizeof l, "%s/l", d);
    snprintf(x, sizeof x, "%s/new", d);
    mkdir(d, 0755);
    int t = open(f, O_CREAT | O_WRONLY, 0644);
    if (t < 0 || write(t, "hello", 5) != 5 || symlink(f, l) != 0) {
        perror("setup");
        return 1;
    }
    close(t);

    show("excl_existing", open(f, O_CREAT | O_EXCL | O_WRONLY, 0644));
    show("excl_new", open(x, O_CREAT | O_EXCL | O_WRONLY, 0644) >= 0);
    show("directory_on_file", open(f, O_RDONLY | O_DIRECTORY));
    show("directory_on_dir", open(d, O_RDONLY | O_DIRECTORY) >= 0);
    show("creat_on_dir", open(d, O_CREAT | O_RDONLY, 0644));
    show("nofollow_symlink", open(l, O_RDONLY | O_NOFOLLOW));
    show("nofollow_file", open(f, O_RDONLY | O_NOFOLLOW) >= 0);

    int ro = open(f, O_RDONLY);
    show("write_rdonly", write(ro, "x", 1));
    show("pwrite_rdonly", pwrite(ro, "x", 1, 0));
    show("read_rdonly", read(ro, b, 1));
    int wo = open(f, O_WRONLY);
    show("read_wronly", read(wo, b, 1));
    show("pread_wronly", pread(wo, b, 1, 0));

    int pp[2], sv[2];
    if (pipe(pp) != 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        perror("pipe/socketpair");
        return 1;
    }
    show("pread_pipe", pread(pp[0], b, 1, 0));
    show("pwrite_pipe", pwrite(pp[1], "x", 1, 0));
    show("lseek_pipe", lseek(pp[0], 0, SEEK_CUR));
    show("pread_socket", pread(sv[0], b, 1, 0));
    show("pwrite_socket", pwrite(sv[0], "x", 1, 0));

    int dn = open("/dev/null", O_RDWR), dz = open("/dev/zero", O_RDONLY), du = open("/dev/urandom", O_RDONLY);
    show("lseek_null_set100", lseek(dn, 100, SEEK_SET));
    show("lseek_zero_set100", lseek(dz, 100, SEEK_SET));
    show("lseek_urandom_set100", lseek(du, 100, SEEK_SET));
    show("lseek_null_badwhence", lseek(dn, 0, 7));
    show("pread_zero", pread(dz, b, 4, 100));

    unlink(x);
    unlink(l);
    unlink(f);
    rmdir(d);
    return 0;
}
