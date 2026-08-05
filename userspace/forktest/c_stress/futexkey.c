/*
 * futexkey.c — does a futex key leak between address spaces?
 *
 * Akuma has no ASLR, so two copies of the same binary place every global at the
 * SAME virtual address. A futex key that is the virtual address alone therefore
 * names one queue shared by every copy of that binary: `FUTEX_WAKE(addr, 1)` in
 * one process pops the FIFO head, which may be a *different* process's waiter.
 * The wrong process gets a spurious wake, the wake is counted as delivered, and
 * the real waiter stays parked forever — a permanent cross-process lost wakeup.
 *
 * This is not hypothetical plumbing: musl's `__tl_lock`/`__tl_unlock` wait and
 * wake on `&__thread_list_lock` (a libc.bss global, fixed VA) with priv=0, and
 * `pthread_create` hands the kernel that same address as the CLONE_CHILD_CLEARTID
 * word. So on a kernel with this defect, *every thread create and exit in every
 * musl process* shares one queue. See docs/runbooks/debug-futex-lost-wakeup.md.
 *
 * Linux does not have the defect: `get_futex_key` only reaches the shared
 * `(inode, index)` form for a page with a `page->mapping`; an anonymous page
 * falls back to `(mm, address)` whether or not FUTEX_PRIVATE was passed. Running
 * this probe on Linux must print the same PASS lines.
 *
 * The test is deterministic — no stress loop, no timing luck:
 *
 *   waiter:  FUTEX_WAIT (non-private) on a .bss global, val it just stored
 *   waker:   a SEPARATE PROCESS running the same binary, so the global is at the
 *            identical VA, issues FUTEX_WAKE (non-private) on that address
 *
 * A correct kernel wakes nobody (woken == 0) because the two globals are
 * different memory. A kernel keying by VA alone reports woken == 1 and steals the
 * other process's wake.
 *
 * Static, musl, no Rust runtime. Build:
 *   aarch64-linux-musl-gcc -O2 -static -o futexkey futexkey.c
 * Run (parent forks the roles itself):
 *   ./futexkey
 */

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_WAIT           0
#define FUTEX_WAKE           1
#define FUTEX_PRIVATE_FLAG 128

/* A .bss global: same virtual address in every process running this binary,
 * and — critically — *different physical memory* in each. */
static volatile uint32_t g_word;

static long futex(volatile uint32_t *u, int op, uint32_t val, const struct timespec *ts) {
    return syscall(SYS_futex, u, op, val, ts, NULL, 0);
}

static void msleep(long ms) {
    struct timespec t = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&t, NULL);
}

static int fails = 0;

static void report(const char *name, int bad, const char *detail) {
    printf("%s %s — %s\n", bad ? "FAIL" : "PASS", name, detail);
    fflush(stdout);
    if (bad) fails++;
}

/* Child role: park in a non-private FUTEX_WAIT on `&g_word` until killed. Writes
 * a byte to `fd` once it is about to enter the syscall so the parent does not
 * race it. Never returns normally. */
static void run_waiter(int fd, int op) {
    g_word = 0x5a5a;
    char c = 'r';
    ssize_t _ = write(fd, &c, 1);
    (void)_;
    /* If the kernel wakes us, say so and exit non-zero: on a correct kernel the
     * peer process's WAKE must not reach this queue, so this call never returns
     * and the parent kills us. */
    long r = futex(&g_word, op, 0x5a5a, NULL);
    printf("    (waiter returned rc=%ld errno=%d — it was woken)\n", r, errno);
    fflush(stdout);
    _exit(3);
}

/* One trial: fork a waiter, let it park, then issue the wake from THIS process
 * (a different address space) at the identical virtual address. Returns the
 * number the kernel claims to have woken. */
static long trial(int wait_op, int wake_op) {
    int pfd[2];
    if (pipe(pfd) != 0) { perror("pipe"); exit(1); }

    pid_t pid = fork();
    if (pid < 0) { perror("fork"); exit(1); }
    if (pid == 0) {
        close(pfd[0]);
        run_waiter(pfd[1], wait_op);
        _exit(4);
    }
    close(pfd[1]);
    char c;
    ssize_t n = read(pfd[0], &c, 1);
    (void)n;
    close(pfd[0]);
    /* The waiter has published "about to call futex"; give it time to actually
     * be enqueued. Generous because a slow guest under load is the normal case
     * here, and an under-slept parent would report a false PASS. */
    msleep(600);

    /* Our own g_word holds whatever this process last put there — deliberately
     * NOT the waiter's value, to prove the wake is not "justified" by matching
     * memory contents. FUTEX_WAKE ignores the value anyway. */
    long woken = futex(&g_word, wake_op, 1, NULL);

    msleep(200);
    kill(pid, SIGKILL);
    int st = 0;
    waitpid(pid, &st, 0);
    return woken;
}

int main(void) {
    printf("=== FUTEXKEY start ===\n");
    fflush(stdout);

    long shared = trial(FUTEX_WAIT, FUTEX_WAKE);
    if (shared == 0) {
        report("shared_wake_stays_in_own_address_space", 0,
               "non-private FUTEX_WAKE woke 0 — peer process's waiter untouched");
    } else {
        char d[160];
        snprintf(d, sizeof d,
                 "non-private FUTEX_WAKE reported woken=%ld — it reached ANOTHER "
                 "process's waiter at the same VA (cross-process lost wakeup)", shared);
        report("shared_wake_stays_in_own_address_space", 1, d);
    }

    /* The private op has been address-space-scoped all along; this arm guards
     * against a fix that accidentally merges the two namespaces the other way. */
    long priv = trial(FUTEX_WAIT | FUTEX_PRIVATE_FLAG, FUTEX_WAKE | FUTEX_PRIVATE_FLAG);
    if (priv == 0) {
        report("private_wake_stays_in_own_address_space", 0,
               "FUTEX_WAKE_PRIVATE woke 0 — as it always should have");
    } else {
        char d[160];
        snprintf(d, sizeof d, "FUTEX_WAKE_PRIVATE reported woken=%ld across processes", priv);
        report("private_wake_stays_in_own_address_space", 1, d);
    }

    /* Mixed: a non-private waiter must not be reachable by a private wake from
     * another process either. */
    long mixed = trial(FUTEX_WAIT, FUTEX_WAKE | FUTEX_PRIVATE_FLAG);
    if (mixed == 0) {
        report("mixed_priv_wake_vs_shared_waiter", 0,
               "private wake did not reach a peer process's non-private waiter");
    } else {
        char d[160];
        snprintf(d, sizeof d, "private wake reached a peer process's waiter, woken=%ld", mixed);
        report("mixed_priv_wake_vs_shared_waiter", 1, d);
    }

    printf("=== FUTEXKEY DONE — %d divergence(s) from Linux ===\n", fails);
    fflush(stdout);
    return fails ? 1 : 0;
}
