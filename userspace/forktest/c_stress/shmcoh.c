/* shmcoh — are two processes' MAP_SHARED mappings of one file coherent, live?
 *
 * They are not on amd64 (2026-10-03): writable MAP_SHARED is demand-paged fills + write-back
 * on munmap/msync, with no page cache, so each mapper has its own frames
 * (docs/reference/subsystems/amd64-shared-write-mmap.md "What this does not promise").
 * SQLite's WAL index is exactly such a mapping, so several processes on one WAL database
 * corrupt it ("database disk image is malformed"). Expect both lines YES once fixed;
 * rc=0 only then.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -o shmcoh shmcoh.c */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>
int main(void) {
    const char *p = "/tmp/shmcoh.dat";
    int fd = open(p, O_RDWR | O_CREAT | O_TRUNC, 0600);
    ftruncate(fd, 32768);
    volatile char *a = mmap(0, 32768, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    a[0] = 1;                       /* parent touches page 0 first */
    int pf[2], pb[2]; pipe(pf); pipe(pb);
    pid_t c = fork();
    if (c == 0) {                   /* child: its own mapping, as an unrelated process would have */
        int fd2 = open(p, O_RDWR);
        volatile char *b = mmap(0, 32768, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 0);
        char x; read(pf[0], &x, 1);                 /* parent has written */
        int saw = b[0] == 7 && b[4096] == 9;
        b[8192] = 5;                                  /* child writes back */
        write(pb[1], "k", 1);
        _exit(saw ? 0 : 1);
    }
    a[0] = 7; a[4096] = 9;
    write(pf[1], "g", 1);
    char y; read(pb[0], &y, 1);
    int parent_sees_child = a[8192] == 5;
    int st; waitpid(c, &st, 0);
    int child_saw = WIFEXITED(st) && WEXITSTATUS(st) == 0;
    printf("child sees parent's live writes: %s\nparent sees child's live write:  %s\n", child_saw ? "YES" : "NO", parent_sees_child ? "YES" : "NO");
    unlink(p);
    return !(child_saw && parent_sees_child);
}
