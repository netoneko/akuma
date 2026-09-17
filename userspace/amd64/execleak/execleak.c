/* execleak — measures PMM behaviour across fork+exec+waitpid cycles.
 *
 * A single-threaded loop: fork, execv /bin/hello (the tree's 13 KiB
 * self-check ELF), waitpid, and every PRINT_EVERY cycles print freeram from
 * sysinfo(2). Flat freeram means teardown returns everything; a slope of
 * ~0.5-1 MB per cycle is the fork child's page-table frames never coming
 * back — the drain that put the FC guest at the OOM floor within one
 * PSTATS sweep of smpstress, and the precondition for the "reads serving
 * zeros under memory pressure" crash family.
 *
 * Build: x86_64-linux-musl-gcc -static -O2 -o execleak execleak.c
 * Run:   INIT=/probes/execleak  (prints EXECLEAK DONE and exits)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sys/sysinfo.h>
#include <errno.h>

#define CYCLES    400
#define PRINT_EVERY 50

int main(void) {
    struct sysinfo si;
    sysinfo(&si);
    printf("execleak: start freeram=%lu MB\n", si.freeram >> 20);
    fflush(stdout);

    for (int n = 1; n <= CYCLES; n++) {
        pid_t pid = fork();
        if (pid < 0) {
            printf("execleak: fork failed at %d errno=%d\n", n, errno);
            fflush(stdout);
            return 1;
        }
        if (pid == 0) {
            char *argv[] = {"hello", NULL};
            execv("/bin/hello", argv);
            _exit(127);
        }
        int st = 0;
        waitpid(pid, &st, 0);
        if (st != 0x7f << 8) {
            printf("execleak: cycle %d bad status 0x%x\n", n, st);
            fflush(stdout);
            return 1;
        }
        if (n % PRINT_EVERY == 0) {
            sysinfo(&si);
            printf("execleak: %d cycles freeram=%lu MB\n", n, si.freeram >> 20);
            fflush(stdout);
        }
    }
    sysinfo(&si);
    printf("execleak: DONE %d cycles freeram=%lu MB\n", CYCLES, si.freeram >> 20);
    fflush(stdout);
    return 0;
}
