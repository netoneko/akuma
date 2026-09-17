/* fs_ops_cost — the ext2 op-cost table, as one binary both kernels can run.
 *
 * The C restatement of `userspace/ext2probe/src/main.rs`'s timed phases
 * (create / seq_write / seq_read / list_dir / delete / mass delete). It exists
 * for the reason `pin_reclaim.c` beside it does: that probe is a `libakuma`
 * binary and `libakuma` does not build for x86_64, so the two architectures
 * could not be compared with the same instrument.
 *
 * Static musl, so the SAME binary runs on Akuma/aarch64, Akuma/amd64 and real
 * Linux. That matters more here than anywhere: this probe's output is a
 * *ratio* between kernels, and a ratio between two different binaries measures
 * the binaries.
 *
 * # Reading the numbers
 *
 * Under QEMU **TCG** an x86_64 guest on an ARM host pays a much larger
 * translation tax than an aarch64 guest on the same host — the guest ISA is the
 * host's in one case and not the other. So the amd64/aarch64 ratio has a floor
 * well above 1.0 that is nothing to do with either kernel, and the only honest
 * reading is *relative*: ops that sit at the floor are not interesting, and ops
 * that sit well above it are. Take the floor from the cheapest ops in the same
 * run rather than assuming a number.
 *
 * `--csv` prints `op,microseconds` for a harness to diff two runs.
 *
 * # Why seq_read is timed twice
 *
 * `seq_read_cold` runs before anything has read the file back; `seq_read_warm`
 * runs immediately after. The difference is the filesystem's block cache, and
 * splitting them is what distinguishes "the disk path is slow" from "the cache
 * is not holding what it should" — two findings with different fixes that a
 * single number cannot tell apart. `ext2probe`'s single `seq_read` is warm.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define BASE_N 25          /* files created and deleted in the timed passes */
#define FILE_SIZE 4096
/* Overridable with `--seq-mb=N`. The default matches `ext2probe`'s 2 MiB, but
 * the interesting sizes are the ones that cross a cache cap: a working set that
 * fits reports the cache's hit path however big the cache is, so a 2 MiB pass
 * cannot tell a 16 MB cache from a 384 MB one. */
#define SEQ_BYTES_DEFAULT (2 * 1024 * 1024)
#define SEQ_CHUNK 4096

static char chunk[SEQ_CHUNK];
static int csv;
static size_t seq_bytes = SEQ_BYTES_DEFAULT;
/* `--repeat=N` runs every phase N times and prints each pass. Take the MINIMUM
 * across passes, not the mean: the noise here is other load on the host, which
 * can only ever ADD time, so the fastest pass is the one least contaminated by
 * it. One sample per boot was the flaw that made the first cross-architecture
 * table unusable — and on a kernel with no `init=` and no reachable sshd, a
 * second sample costs a whole reboot unless the probe does it itself. */
static int repeat = 1;

/* `CLOCK_MONOTONIC`, not `gettimeofday`. A NIC-less boot never runs SNTP, so
 * `CLOCK_REALTIME` is "never synced" and reads a constant 0 — which makes every
 * phase measure 0 us and the whole table look instantaneous. The monotonic
 * clock is uptime, which the kernel always has. */
static long long now_us(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void emit(const char *op, long long us) {
    if (csv) {
        printf("%s,%lld\n", op, us);
    } else {
        printf("FS_OPS: %-14s %8lld us\n", op, us);
    }
    fflush(stdout);
}

/* Create `n` files of `size` bytes. Returns microseconds. */
static long long create_files(const char *dir, int n, size_t size) {
    long long t0 = now_us();
    for (int i = 0; i < n; i++) {
        char p[320];
        snprintf(p, sizeof p, "%s/f%d", dir, i);
        int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd < 0) {
            continue;
        }
        for (size_t done = 0; done < size; done += SEQ_CHUNK) {
            size_t want = size - done < SEQ_CHUNK ? size - done : SEQ_CHUNK;
            if (write(fd, chunk, want) != (ssize_t)want) {
                break;
            }
        }
        close(fd);
    }
    return now_us() - t0;
}

static long long delete_files(const char *dir, int n) {
    long long t0 = now_us();
    for (int i = 0; i < n; i++) {
        char p[320];
        snprintf(p, sizeof p, "%s/f%d", dir, i);
        unlink(p);
    }
    return now_us() - t0;
}

static long long seq_write(const char *path, size_t total) {
    long long t0 = now_us();
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    for (size_t done = 0; done < total; done += SEQ_CHUNK) {
        if (write(fd, chunk, SEQ_CHUNK) != SEQ_CHUNK) {
            break;
        }
    }
    close(fd);
    return now_us() - t0;
}

/* Read `path` whole. `*got` receives the byte count, so a short read is
 * visible rather than showing up as a suspiciously fast time. */
static long long seq_read(const char *path, size_t *got) {
    static char rbuf[SEQ_CHUNK];
    *got = 0;
    long long t0 = now_us();
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    for (;;) {
        ssize_t n = read(fd, rbuf, sizeof rbuf);
        if (n <= 0) {
            break;
        }
        *got += (size_t)n;
    }
    close(fd);
    return now_us() - t0;
}

static long long list_dir(const char *path, int *count) {
    *count = 0;
    long long t0 = now_us();
    DIR *d = opendir(path);
    if (!d) {
        return -1;
    }
    while (readdir(d)) {
        (*count)++;
    }
    closedir(d);
    return now_us() - t0;
}

int main(int argc, char **argv) {
    const char *root = "/tmp";
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--csv")) {
            csv = 1;
        } else if (!strncmp(argv[i], "--repeat=", 9)) {
            repeat = atoi(argv[i] + 9);
            if (repeat < 1) {
                repeat = 1;
            }
        } else if (!strncmp(argv[i], "--seq-mb=", 9)) {
            seq_bytes = (size_t)atoi(argv[i] + 9) * 1024 * 1024;
        } else {
            root = argv[i];
        }
    }
    memset(chunk, 'x', sizeof chunk);

    char dir[192];
    snprintf(dir, sizeof dir, "%s/fsops", root);
    mkdir(dir, 0755);

    if (!csv) {
        printf("FS_OPS: start root=%s base_n=%d seq_bytes=%zu\n", root, BASE_N,
               seq_bytes);
    }

    char big[256];
    snprintf(big, sizeof big, "%s/big", dir);

    for (int pass = 0; pass < repeat; pass++) {
        if (repeat > 1 && !csv) {
            printf("FS_OPS: --- pass %d of %d ---\n", pass + 1, repeat);
        }
        emit("create", create_files(dir, BASE_N, FILE_SIZE));
        emit("seq_write", seq_write(big, seq_bytes));

        /* Cold first, then warm: see the header on why this is two numbers.
         * Only pass 1's "cold" is genuinely cold — later passes rewrote the
         * file, so its blocks are already resident. Read the cold column from
         * the first pass and the warm column from the minimum of all. */
        size_t got = 0;
        emit("seq_read_cold", seq_read(big, &got));
        if (got != seq_bytes) {
            printf("FS_OPS: WARNING seq_read got %zu of %zu bytes\n", got, seq_bytes);
        }
        emit("seq_read_warm", seq_read(big, &got));

        int listed = 0;
        emit("list_dir", list_dir(dir, &listed));
        if (!csv && pass == 0) {
            printf("FS_OPS: list_dir saw %d entries\n", listed);
        }

        emit("delete", delete_files(dir, BASE_N));
        unlink(big);
    }
    rmdir(dir);

    if (!csv) {
        printf("FS_OPS: done\n");
    }
    return 0;
}
