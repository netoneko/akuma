/*
 * dynspawn — hammer vfork+exec of a DYNAMICALLY linked binary and check that
 * the dynamic loader gets each child to `main`.
 *
 * Targets the second crash class in the -j4 self-host logs
 * (docs/runbooks/debug-thread-spawn-segv.md §3): freshly-exec'd processes taking
 * an instruction abort at `ld-musl+0x6c964` — a function prologue reached from
 * `_dlstart_c` calling `__dls2` — with the PC gaining exactly `INTERP_BASE`
 * (0x30000000) per occurrence. That is ld-musl's own `R_AARCH64_RELATIVE`
 * self-relocation being applied to data that was *already* relocated.
 *
 * Both halves of the shape matter and both are here:
 *   - the PARENT is dynamically linked too, because in the failing case
 *     (cargo -> rustc) it is, and a vfork child shares the parent's address
 *     space until it execs — so the parent's own relocated ld-musl data is what
 *     is at risk.
 *   - the spawn goes through posix_spawn, which musl implements with
 *     CLONE_VM|CLONE_VFORK, i.e. Akuma's vfork fastpath (`vfork_process`).
 *
 * After every spawn the parent re-checks its OWN relocated pointer and makes a
 * PLT call, so corruption of the parent's GOT is reported rather than being
 * left to crash the process silently later.
 *
 * Usage: dynspawn [spawns-per-thread] [threads] [child-path] [expected-status] [child-arg]
 *
 * Point it at a *large* dynamic binary to add demand-paging pressure to the mix,
 * which is what the failing case actually looked like:
 *   dynspawn 40 4 /usr/local/bin/rustc 0 --version
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static const char self_msg[] = "akuma-dynspawn";
static const char *const self_relocated = self_msg;

static _Atomic unsigned long n_ok, n_spawn_err, n_bad_status, n_signaled, n_self_corrupt;
static _Atomic unsigned long first_sig, first_status;
static unsigned long iters = 200;
static const char *child_path = "/tmp/dynchild";
static int want_status = 42;
static char *child_arg;

static void *worker(void *v)
{
    (void)v;
    char *argv[] = {(char *)child_path, child_arg, NULL};

    for (unsigned long i = 0; i < iters; i++) {
        pid_t pid;
        int rc = posix_spawn(&pid, child_path, NULL, NULL, argv, environ);
        if (rc != 0) {
            atomic_fetch_add(&n_spawn_err, 1);
            continue;
        }
        int status = 0;
        if (waitpid(pid, &status, 0) < 0) {
            atomic_fetch_add(&n_spawn_err, 1);
            continue;
        }
        if (WIFSIGNALED(status)) {
            atomic_fetch_add(&n_signaled, 1);
            unsigned long z = 0;
            atomic_compare_exchange_strong(&first_sig, &z, (unsigned long)WTERMSIG(status));
        } else if (!WIFEXITED(status) || WEXITSTATUS(status) != want_status) {
            atomic_fetch_add(&n_bad_status, 1);
            unsigned long z = 0;
            atomic_compare_exchange_strong(&first_status, &z,
                                           (unsigned long)WEXITSTATUS(status) + 1);
        } else {
            atomic_fetch_add(&n_ok, 1);
        }

        /* Our own image still intact? A GOT that gained a stray `+= base` shows
         * up here before it can send us somewhere unexecutable. */
        if (self_relocated != self_msg || strlen(self_relocated) != 14)
            atomic_fetch_add(&n_self_corrupt, 1);
    }
    return NULL;
}

int main(int argc, char **argv)
{
    unsigned long nthreads = 4;
    if (argc > 1) iters = strtoul(argv[1], NULL, 0);
    if (argc > 2) nthreads = strtoul(argv[2], NULL, 0);
    if (argc > 3) child_path = argv[3];
    if (argc > 4) want_status = atoi(argv[4]);
    if (argc > 5) child_arg = argv[5];
    if (nthreads > 16) nthreads = 16;

    printf("dynspawn: %lu threads x %lu posix_spawn(%s %s) [vfork+exec of a dynamic ELF], want status %d\n",
           nthreads, iters, child_path, child_arg ? child_arg : "", want_status);
    fflush(stdout);

    pthread_t th[16];
    unsigned long started = 0;
    for (unsigned long k = 0; k < nthreads; k++) {
        if (pthread_create(&th[k], NULL, worker, NULL) != 0) break;
        started++;
    }
    for (unsigned long k = 0; k < started; k++) pthread_join(th[k], NULL);

    unsigned long ok = atomic_load(&n_ok);
    unsigned long sig = atomic_load(&n_signaled);
    unsigned long bad = atomic_load(&n_bad_status);
    unsigned long err = atomic_load(&n_spawn_err);
    unsigned long self = atomic_load(&n_self_corrupt);

    printf("  children reaching main : %lu\n", ok);
    printf("  killed by a signal     : %lu%s", sig, sig ? "" : "\n");
    if (sig) printf("  (first signal: %lu)\n", atomic_load(&first_sig));
    printf("  wrong exit status      : %lu%s", bad, bad ? "" : "\n");
    if (bad) printf("  (first status: %lu)\n", atomic_load(&first_status) - 1);
    printf("  posix_spawn/wait errors: %lu\n", err);
    printf("  parent self-relocation corrupt: %lu\n", self);

    unsigned long div = sig + bad + self;
    printf("=== DYNSPAWN DONE — %lu divergence(s) ===\n", div);
    return div ? 1 : 0;
}
