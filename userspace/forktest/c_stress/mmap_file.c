/*
 * mmap_file.c — file-backed mmap + touch, used by the kernel boot suite to prove
 * that mapping a file larger than RAM SIGSEGVs *this* process instead of panicking
 * the whole kernel (docs/LLAMA_MMAP_OOM_KERNEL_ABORT.md).
 *
 *   fd   = open(path, O_RDONLY)
 *   size = fstat(fd).st_size
 *   p    = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0)
 *   for off in 0..size step PAGE: sink += *(volatile char*)(p + off)
 *
 * Touching one byte per page drives the kernel's file-backed demand-paging /
 * readahead path. When the file is larger than free RAM the kernel runs out of
 * pages mid-touch and must kill us with SIGSEGV (exit code -11 as seen by the
 * parent) — the kernel itself must stay up. If the file fits, we read it all and
 * exit 0. We never write, so MAP_PRIVATE is just a read-only view of the file.
 *
 * Static, musl, no Go runtime — a pure-C control so a crash is unambiguously the
 * kernel's fault.
 *
 * Usage: mmap_file /models/qwen3.5-0.8b-q4.gguf
 */

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define PAGE_SIZE 4096UL

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "mmap_file: usage: mmap_file <path>\n");
        return 2;
    }
    const char *path = argv[1];

    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        fprintf(stderr, "mmap_file: open(%s) failed\n", path);
        return 2;
    }

    struct stat st;
    if (fstat(fd, &st) != 0 || st.st_size <= 0) {
        fprintf(stderr, "mmap_file: fstat(%s) failed\n", path);
        close(fd);
        return 2;
    }
    size_t size = (size_t)st.st_size;

    void *p = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (p == MAP_FAILED) {
        fprintf(stderr, "mmap_file: mmap(%zu) failed\n", size);
        close(fd);
        return 2;
    }

    /* Sentinel so the kernel log shows we got past mmap and into the touch loop
     * (the SIGSEGV, if it comes, lands somewhere in here). */
    fprintf(stdout, "mmap_file: mapped %zu bytes of %s, touching every page\n", size, path);
    fflush(stdout);

    volatile unsigned long sink = 0;
    const volatile unsigned char *base = (const volatile unsigned char *)p;
    for (size_t off = 0; off < size; off += PAGE_SIZE) {
        sink += base[off];
    }

    fprintf(stdout, "mmap_file: touched all pages, sink=%lu (file fit in RAM)\n",
            (unsigned long)sink);
    fflush(stdout);

    munmap(p, size);
    close(fd);
    return 0;
}
