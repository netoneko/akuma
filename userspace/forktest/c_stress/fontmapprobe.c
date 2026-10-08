/*
 * fontmapprobe: what a renderer does with a font file the browser pointed it
 * at: open it, mmap it read-only (MAP_PRIVATE and MAP_SHARED), and hand the
 * bytes to FreeType. On Akuma + Chromium 152 no system font ever loads (every
 * family measures width 0, `CSS.getPlatformFontsForNode` is empty) although
 * every open() succeeds, so the bytes are the suspect.
 *
 * For each regular file in the directory argument (default /usr/share/fonts/
 * dejavu): read it whole, then compare against a MAP_PRIVATE map, a MAP_SHARED
 * map, a map at a nonzero page offset, a pread, and the same maps taken in a
 * forked child (a different address space opening the same inode). Prints
 * the first mismatch's offset. Linux passes all; the font tables begin with
 * the sfnt magic, which is also checked.
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
#include <sys/wait.h>
#include <unistd.h>

static int fails, files;
static long first_diff(const unsigned char *a, const unsigned char *b, long n) {
    for (long i = 0; i < n; i++) if (a[i] != b[i]) return i;
    return -1;
}
static int compare(const char *path, const unsigned char *ref, long size, const char *who) {
    int bad = 0;
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) { printf("fontmapprobe: %s %s open failed errno %d FAIL\n", who, path, errno); return 1; }
    int flags[2] = { MAP_PRIVATE, MAP_SHARED };
    const char *names[2] = { "MAP_PRIVATE", "MAP_SHARED" };
    for (int k = 0; k < 2; k++) {
        unsigned char *m = mmap(0, size, PROT_READ, flags[k], fd, 0);
        if (m == MAP_FAILED) { printf("fontmapprobe: %s %s %s mmap failed errno %d FAIL\n", who, path, names[k], errno); bad = 1; continue; }
        long d = first_diff(m, ref, size);
        if (d >= 0) { printf("fontmapprobe: %s %s %s differs at %ld (map %02x file %02x) FAIL\n", who, path, names[k], d, m[d], ref[d]); bad = 1; }
        munmap(m, size);
    }
    if (size > 8192) {
        long off = 4096;
        unsigned char *m = mmap(0, size - off, PROT_READ, MAP_PRIVATE, fd, off);
        if (m == MAP_FAILED) { printf("fontmapprobe: %s %s offset map failed errno %d FAIL\n", who, path, errno); bad = 1; }
        else {
            long d = first_diff(m, ref + off, size - off);
            if (d >= 0) { printf("fontmapprobe: %s %s offset-4096 map differs at %ld FAIL\n", who, path, d); bad = 1; }
            munmap(m, size - off);
        }
    }
    unsigned char *p = malloc(size);
    ssize_t r = pread(fd, p, size, 0);
    if (r != size || first_diff(p, ref, size) >= 0) { printf("fontmapprobe: %s %s pread mismatch (%zd of %ld) FAIL\n", who, path, r, size); bad = 1; }
    free(p);
    close(fd);
    return bad;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/usr/share/fonts/dejavu";
    DIR *d = opendir(dir);
    if (!d) { perror("opendir"); return 1; }
    struct dirent *e;
    while ((e = readdir(d))) {
        char path[512];
        snprintf(path, sizeof path, "%s/%s", dir, e->d_name);
        struct stat st;
        size_t nl = strlen(e->d_name);
        if (nl < 5 || (strcmp(e->d_name + nl - 4, ".ttf") && strcmp(e->d_name + nl - 4, ".otf") && strcmp(e->d_name + nl - 4, ".ttc"))) continue;
        if (stat(path, &st) || !S_ISREG(st.st_mode) || st.st_size < 4096) continue;
        int fd = open(path, O_RDONLY);
        if (fd < 0) continue;
        unsigned char *ref = malloc(st.st_size);
        long got = 0;
        while (got < st.st_size) { ssize_t r = read(fd, ref + got, st.st_size - got); if (r <= 0) break; got += r; }
        close(fd);
        if (got != st.st_size) { printf("fontmapprobe: %s short read %ld of %ld FAIL\n", path, got, (long)st.st_size); fails++; free(ref); continue; }
        files++;
        int magic = !memcmp(ref, "\0\1\0\0", 4) || !memcmp(ref, "OTTO", 4) || !memcmp(ref, "ttcf", 4);
        if (!magic) { printf("fontmapprobe: %s no sfnt magic (%02x%02x%02x%02x) FAIL\n", path, ref[0], ref[1], ref[2], ref[3]); fails++; }
        fails += compare(path, ref, st.st_size, "parent");
        pid_t c = fork();
        if (c == 0) { _exit(compare(path, ref, st.st_size, "child") ? 1 : 0); }
        int status = 0;
        waitpid(c, &status, 0);
        if (!WIFEXITED(status) || WEXITSTATUS(status)) fails++;
        free(ref);
    }
    printf("fontmapprobe: %d files, %s\n", files, fails ? "FAILED" : "all ok");
    return fails != 0;
}
