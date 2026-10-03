/* fbprobe — /dev/fb0 through the Linux fbdev ABI, raw syscalls only.
 *
 * The kernel side is amd64/src/fbdev.rs + crates/akuma-fbdev (2026-10-03). Same binary runs on a
 * Linux box with a framebuffer, which is how its expected output is calibrated. Prints a rung
 * before each step so a hang names itself; exit 0 only if every rung passed.
 *
 *   1  open /dev/fb0 O_RDWR
 *   2  FBIOGET_VSCREENINFO / FBIOGET_FSCREENINFO, printed in full
 *   3  FBIOPUT_VSCREENINFO with exactly what GET said succeeds; a different depth is EINVAL
 *   4  FBIOPAN_DISPLAY to (0,0) succeeds
 *   5  mmap MAP_SHARED PROT_READ|PROT_WRITE of smem_len; MAP_PRIVATE is refused
 *   6  full-screen fill, timed: MB/s near the kernel's [fb] clear figure means write-combining
 *   7  read back a few pixels of a gradient
 *   8  a second process gets EBUSY (Akuma: one owner at a time; Linux allows it — reported, not scored)
 *   9  munmap, close
 *
 * With an argument "hold N" it keeps the gradient on screen for N seconds before releasing it.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -o fbprobe fbprobe.c */
#include <errno.h>
#include <fcntl.h>
#include <linux/fb.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails;
static void rung(int n, const char *w) { printf("rung %d: %s ... ", n, w); fflush(stdout); }
static void verdict(int ok) { printf("%s\n", ok ? "PASS" : "FAIL"); fflush(stdout); if (!ok) fails++; }
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

int main(int argc, char **argv) {
    int hold = (argc > 2 && strcmp(argv[1], "hold") == 0) ? atoi(argv[2]) : 0;
    struct utsname u; uname(&u);
    int akuma = strcmp(u.sysname, "Akuma") == 0;

    rung(1, "open /dev/fb0");
    int fd = open("/dev/fb0", O_RDWR | O_CLOEXEC);
    verdict(fd >= 0);
    if (fd < 0) { printf("  errno %d (%s)\n", errno, strerror(errno)); return 1; }

    rung(2, "FBIOGET_VSCREENINFO + FBIOGET_FSCREENINFO");
    struct fb_var_screeninfo v; struct fb_fix_screeninfo f;
    int ok = ioctl(fd, FBIOGET_VSCREENINFO, &v) == 0 && ioctl(fd, FBIOGET_FSCREENINFO, &f) == 0;
    verdict(ok && v.xres && v.yres && f.line_length >= v.xres * (v.bits_per_pixel / 8));
    printf("  var: %ux%u virt %ux%u off %u,%u bpp %u r%u/%u g%u/%u b%u/%u t%u/%u vmode %u\n",
           v.xres, v.yres, v.xres_virtual, v.yres_virtual, v.xoffset, v.yoffset, v.bits_per_pixel,
           v.red.offset, v.red.length, v.green.offset, v.green.length, v.blue.offset, v.blue.length,
           v.transp.offset, v.transp.length, v.vmode);
    printf("  fix: id '%.16s' smem 0x%lx+%u line %u type %u visual %u\n",
           f.id, f.smem_start, f.smem_len, f.line_length, f.type, f.visual);
    if (!ok) return 1;

    rung(3, "FBIOPUT_VSCREENINFO: same mode accepted, other depth EINVAL");
    struct fb_var_screeninfo same = v, other = v;
    int put_same = ioctl(fd, FBIOPUT_VSCREENINFO, &same);
    other.bits_per_pixel = v.bits_per_pixel == 16 ? 32 : 16;
    int put_other = ioctl(fd, FBIOPUT_VSCREENINFO, &other);
    int e_other = errno;
    verdict(put_same == 0 && put_other == -1 && e_other == EINVAL);

    rung(4, "FBIOPAN_DISPLAY to the origin");
    struct fb_var_screeninfo pan = v; pan.xoffset = pan.yoffset = 0;
    verdict(ioctl(fd, FBIOPAN_DISPLAY, &pan) == 0);

    rung(5, "mmap MAP_SHARED (MAP_PRIVATE refused)");
    void *priv = mmap(0, f.smem_len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if (priv != MAP_FAILED) munmap(priv, f.smem_len);
    uint8_t *fb = mmap(0, f.smem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    /* Linux fbdev allows MAP_PRIVATE of most drivers; Akuma refuses it. Scored only on Akuma. */
    verdict(fb != MAP_FAILED && (!akuma || priv == MAP_FAILED));
    if (fb == MAP_FAILED) { printf("  errno %d\n", errno); return 1; }

    rung(6, "full-screen fill, timed");
    size_t bytes = (size_t)f.line_length * v.yres;
    double t0 = now();
    for (int rep = 0; rep < 4; rep++) {
        for (unsigned y = 0; y < v.yres; y++) {
            uint32_t *row = (uint32_t *)(fb + (size_t)y * f.line_length);
            uint32_t px = rep & 1 ? 0x00200040u : 0x00402000u;
            for (unsigned x = 0; x < v.xres; x++) row[x] = px;
        }
    }
    double dt = now() - t0;
    printf("(%.0f MB/s) ", 4.0 * bytes / 1e6 / dt);
    verdict(dt > 0);

    rung(7, "gradient + read back");
    for (unsigned y = 0; y < v.yres; y++) {
        uint32_t *row = (uint32_t *)(fb + (size_t)y * f.line_length);
        for (unsigned x = 0; x < v.xres; x++)
            row[x] = ((x * 255 / v.xres) << v.red.offset) | ((y * 255 / v.yres) << v.green.offset) | (0x80u << v.blue.offset);
    }
    uint32_t *mid = (uint32_t *)(fb + (size_t)(v.yres / 2) * f.line_length);
    uint32_t want = (((v.xres / 2) * 255 / v.xres) << v.red.offset) | (((v.yres / 2) * 255 / v.yres) << v.green.offset) | (0x80u << v.blue.offset);
    verdict(mid[v.xres / 2] == want);

    rung(8, "a second process opening /dev/fb0");
    pid_t c = fork();
    if (c == 0) {
        int fd2 = open("/dev/fb0", O_RDWR);
        _exit(fd2 < 0 && errno == EBUSY ? 0 : fd2 >= 0 ? 2 : 1);
    }
    int st; waitpid(c, &st, 0);
    int code = WIFEXITED(st) ? WEXITSTATUS(st) : 99;
    printf("(%s) ", code == 0 ? "EBUSY" : code == 2 ? "opened" : "other error");
    /* Akuma: one owner -> EBUSY. Linux: any number of openers -> opened. */
    verdict(akuma ? code == 0 : code == 2);

    if (hold > 0) { printf("  holding the screen for %d s\n", hold); fflush(stdout); sleep(hold); }

    rung(9, "munmap + close");
    verdict(munmap(fb, f.smem_len) == 0 && close(fd) == 0);

    printf("fbprobe: %s (%d failing)\n", fails ? "FAIL" : "PASS", fails);
    return fails != 0;
}
