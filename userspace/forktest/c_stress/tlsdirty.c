/* tlsdirty.c — does a freshly spawned thread ever see NON-ZERO thread-local storage?
 *
 * Targets the precondition behind rustc's `-j4` abort on Akuma:
 *
 *   fatal runtime error: current thread handle already set during thread spawn
 *
 * That is Rust std's `rtabort!` in library/std/src/thread/lifecycle.rs:162, reached
 * when `set_current` (library/std/src/thread/current.rs:121) finds EITHER its
 * `CURRENT` thread-local pointer non-null, OR a thread-id thread-local already set
 * to a different id. Both live in TLS. musl's `__copy_tls` on aarch64 memcpy's only
 * `.tdata` and relies on the fresh `mmap(MAP_ANONYMOUS)` backing `.tbss` being
 * zero-filled by the kernel. So the abort reduces to a kernel-contract violation:
 * a new thread read non-zero from storage that had to arrive as zeros — or its
 * TPIDR_EL0 aimed at another thread's block.
 *
 * This probe reproduces that contract with no Rust runtime in the way:
 *
 *   phase 1  churn: spawn/join one thread at a time. Each checks its own .tbss
 *            reads zero, then scribbles a sentinel and exits. If the kernel ever
 *            recycles a TLS VA without re-zeroing, a later thread reads the
 *            sentinel and we name the exact iteration.
 *   phase 2  fan-out: K threads live at once, each publishing &tls_current. Two
 *            live threads reporting the SAME address means TLS aliasing (a
 *            TPIDR_EL0 / thread-pointer bug) rather than a dirty page — the other
 *            way `set_current` can see a populated slot.
 *   phase 3  churn under fan-out: both at once, which is what `-j4` actually does.
 *
 * Sentinels are distinct per phase so the report says which prior life dirtied it.
 *
 * Build on the host (in-VM compilers are the chicken-and-egg problem in
 * AKUMA_SELF_HOSTING.md §7g):
 *   aarch64-linux-musl-gcc -O2 -static -o tlsdirty tlsdirty.c
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

#define SENTINEL_CURRENT 0xC0FFEE00DEADBEEFULL
#define SENTINEL_ID      0x1234ABCD5678EF90ULL
#define FANOUT           24
#define CHURN            2000

/* Mimics std's CURRENT + thread-id locals: .tbss, must arrive zeroed. */
static __thread unsigned long long tls_current;
static __thread unsigned long long tls_id;
/* A block big enough to span more than one word, to catch partial zeroing. */
static __thread unsigned long long tls_pad[16];

static volatile int dirty_hits = 0;
static volatile int alias_hits = 0;
static volatile int hold = 1;

static void *addr_slot[FANOUT];

static void check_and_dirty(const char *phase, long iter)
{
    /* THE CHECK: everything here must read zero on a brand-new thread. */
    if (tls_current != 0 || tls_id != 0) {
        __atomic_add_fetch(&dirty_hits, 1, __ATOMIC_SEQ_CST);
        printf("[%s] DIRTY TLS at iter %ld: tls_current=%#llx tls_id=%#llx (&tls_current=%p)\n",
               phase, iter, tls_current, tls_id, (void *)&tls_current);
        if (tls_current == SENTINEL_CURRENT)
            printf("        ^ exactly a previous thread's sentinel — VA recycled without re-zeroing\n");
        fflush(stdout);
    }
    for (int i = 0; i < 16; i++) {
        if (tls_pad[i] != 0) {
            __atomic_add_fetch(&dirty_hits, 1, __ATOMIC_SEQ_CST);
            printf("[%s] DIRTY TLS pad[%d]=%#llx at iter %ld\n", phase, i, tls_pad[i], iter);
            fflush(stdout);
            break;
        }
    }

    /* Scribble, so a non-rezeroed reuse is unmistakable rather than plausible noise. */
    tls_current = SENTINEL_CURRENT;
    tls_id = SENTINEL_ID;
    for (int i = 0; i < 16; i++)
        tls_pad[i] = SENTINEL_CURRENT ^ (unsigned long long)i;
}

static void *churn_thread(void *arg)
{
    check_and_dirty("1", (long)arg);
    return NULL;
}

static void *fanout_thread(void *arg)
{
    long idx = (long)arg;
    check_and_dirty("2", idx);
    addr_slot[idx] = (void *)&tls_current;
    /* Stay alive so every slot is concurrently live when main compares them. */
    while (__atomic_load_n(&hold, __ATOMIC_SEQ_CST))
        usleep(2000);
    return NULL;
}

static void *mixed_thread(void *arg)
{
    check_and_dirty("3", (long)arg);
    return NULL;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("=== TLSDIRTY start (pid %d) ===\n", (int)getpid());

    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 * 1024);

    /* ---- phase 1: sequential churn, maximal TLS VA reuse ---- */
    printf("[1] %d sequential spawn/join, checking TLS is zero each time\n", CHURN);
    for (long i = 0; i < CHURN; i++) {
        pthread_t th;
        int rc = pthread_create(&th, &attr, churn_thread, (void *)i);
        if (rc != 0) {
            printf("[1] spawn refused at iter %ld: rc=%d (%s) — capacity, not a TLS fault\n",
                   i, rc, strerror(rc));
            usleep(100000);
            continue;
        }
        pthread_join(th, NULL);
    }
    printf("[1] done, dirty_hits=%d\n", dirty_hits);

    /* ---- phase 2: fan-out, check no two live threads share a TLS address ---- */
    printf("[2] fan-out %d live threads, checking for TLS aliasing\n", FANOUT);
    pthread_t f[FANOUT];
    int nf = 0;
    memset(addr_slot, 0, sizeof(addr_slot));
    for (long i = 0; i < FANOUT; i++) {
        if (pthread_create(&f[nf], &attr, fanout_thread, (void *)i) == 0)
            nf++;
    }
    usleep(300000);
    for (int i = 0; i < nf; i++) {
        if (!addr_slot[i]) continue;
        for (int j = i + 1; j < nf; j++) {
            if (addr_slot[i] && addr_slot[i] == addr_slot[j]) {
                __atomic_add_fetch(&alias_hits, 1, __ATOMIC_SEQ_CST);
                printf("[2] TLS ALIAS: live threads %d and %d share &tls_current=%p\n",
                       i, j, addr_slot[i]);
            }
        }
    }
    __atomic_store_n(&hold, 0, __ATOMIC_SEQ_CST);
    for (int i = 0; i < nf; i++)
        pthread_join(f[i], NULL);
    printf("[2] done (%d live), alias_hits=%d\n", nf, alias_hits);

    /* ---- phase 3: churn while a fan-out is live (what -j4 actually does) ---- */
    printf("[3] churn under concurrent fan-out\n");
    hold = 1;
    nf = 0;
    for (long i = 0; i < FANOUT / 2; i++) {
        if (pthread_create(&f[nf], &attr, fanout_thread, (void *)i) == 0)
            nf++;
    }
    for (long i = 0; i < CHURN / 2; i++) {
        pthread_t th;
        if (pthread_create(&th, &attr, mixed_thread, (void *)i) != 0) {
            usleep(50000);
            continue;
        }
        pthread_join(th, NULL);
    }
    __atomic_store_n(&hold, 0, __ATOMIC_SEQ_CST);
    for (int i = 0; i < nf; i++)
        pthread_join(f[i], NULL);
    pthread_attr_destroy(&attr);

    printf("=== TLSDIRTY DONE — dirty_hits=%d alias_hits=%d ===\n", dirty_hits, alias_hits);
    return (dirty_hits || alias_hits) ? 1 : 0;
}
