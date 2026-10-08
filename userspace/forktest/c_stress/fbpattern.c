/*
 * fbpattern: does a user mapping of /dev/fb0 reach the screen? Prints the fbdev
 * geometry, mmaps the pixels the way kami does, paints a pattern a person can
 * recognise (top third red / green / blue bars, a white band, a left-to-right
 * grey ramp, a 100 px white frame around the edge), reads two pixels back, and
 * holds it for N seconds (default 30). Nothing here can be scored by a program:
 * the verdict is what the panel shows. Exit 0 if the mapping worked.
 * Written 2026-10-08 when kami blitted a frame on the trashcan's metal and the
 * screen went black.
 */
#include <fcntl.h>
#include <linux/fb.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

int main(int argc, char **argv) {
    int hold = argc > 1 ? atoi(argv[1]) : 30;
    int fd = open("/dev/fb0", O_RDWR);
    if (fd < 0) { perror("open /dev/fb0"); return 1; }
    struct fb_var_screeninfo v; struct fb_fix_screeninfo f;
    if (ioctl(fd, FBIOGET_VSCREENINFO, &v) || ioctl(fd, FBIOGET_FSCREENINFO, &f)) { perror("ioctl"); return 1; }
    printf("fbpattern: %ux%u virt %ux%u bpp %u stride %u smem_len %u red@%u/%u green@%u/%u blue@%u/%u\n",
           v.xres, v.yres, v.xres_virtual, v.yres_virtual, v.bits_per_pixel, f.line_length, f.smem_len,
           v.red.offset, v.red.length, v.green.offset, v.green.length, v.blue.offset, v.blue.length);
    size_t len = (size_t)f.line_length * v.yres;
    uint8_t *m = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) { perror("mmap"); return 1; }
    for (unsigned y = 0; y < v.yres; y++) {
        uint32_t *row = (uint32_t *)(m + (size_t)y * f.line_length);
        for (unsigned x = 0; x < v.xres; x++) {
            uint32_t c;
            if (x < 100 || y < 100 || x >= v.xres - 100 || y >= v.yres - 100) c = 0xFFFFFF;
            else if (y < v.yres / 3) c = x < v.xres / 3 ? 0xFF0000 : x < 2 * v.xres / 3 ? 0x00FF00 : 0x0000FF;
            else if (y < v.yres / 3 + 200) c = 0xFFFFFF;
            else { unsigned g = x * 255 / v.xres; c = g << 16 | g << 8 | g; }
            row[x] = c;
        }
    }
    uint32_t a = *(uint32_t *)(m + (size_t)200 * f.line_length + 4 * 200);
    uint32_t b = *(uint32_t *)(m + (size_t)(v.yres - 300) * f.line_length + 4 * (v.xres - 300));
    printf("fbpattern: readback (200,200)=%08x want ff0000, (-300,-300)=%08x\n", a, b);
    printf("fbpattern: holding %d s\n", hold);
    fflush(stdout);
    sleep(hold);
    return a == 0xFF0000 ? 0 : 2;
}
