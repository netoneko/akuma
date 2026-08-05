/*
 * clonearg — does a freshly-cloned thread see the memory its parent wrote
 * immediately before `clone()`?
 *
 * Deterministic probe for the 2026-08-05 self-host blocker
 * (docs/runbooks/debug-thread-spawn-segv.md). Symbolizing the rustc crash showed
 * the victim dies on the FIRST thing a new thread does:
 *
 *     thread_start:
 *         ldr  x20, [x0]     ; x0 = the clone argument, popped off the child stack
 *         ...
 *         bl   __aarch64_ldadd8_relax   ; refcount fetch_add(1) at [x20]  <- SIGSEGV
 *
 * The child had read a value that was not the pointer its parent stored. Two
 * memories are involved and they fail differently, so this probe reads both:
 *
 *   1. the child's own STACK — musl's `__clone` has the parent `stp fn,arg`
 *      just below the child's SP, and the child pops it with `ldp x1,x0,[sp],#16`
 *      as its first instruction. Stale here and the child gets a *previous*
 *      thread's argument: still a plausible pointer, now freed. This is why the
 *      observed faulting addresses are small integers (0x0, 0x5, 0x7, 0x7fff0)
 *      rather than garbage — freed heap reused by something else.
 *   2. the ARGUMENT BLOCK the parent filled in before cloning.
 *
 * Rather than use pthread_create (whose child runs musl's `start`, and would
 * simply SIGSEGV on stale input like rustc does), this clones raw so the child's
 * first instructions are ours and every check is non-destructive: the argument
 * pointer is range-checked against a static pool before any dereference, so a
 * stale value is *reported*, never crashed on.
 *
 * A correct kernel prints "0 divergence(s)". Calibrate the probe itself on real
 * Linux, where it must also print 0:
 *   docker run --rm --platform linux/arm64 -v "$PWD/clonearg:/clonearg:ro" alpine /clonearg
 *
 * Usage: clonearg [iterations-per-spawner] [spawners]
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define MAX_SPAWNERS 8
#define POOL_PER_SPAWNER 8
#define GUARD_SIZE 8192
/* Sized and allocated the way musl's pthread_create does it — see `spawner`. */
#define STACK_SIZE (128 * 1024)

#define MAGIC 0x5afe7ea5c10fe00dULL
#define SENT1 0xc0ffee0000000001ULL
#define SENT2 0xdeadbeef00000002ULL

/* Reserved words the parent writes below the fn/arg pair, read by the child
 * straight off its stack. Offsets are from the child's SP on entry. */
#define OFF_SENT1 24
#define OFF_SENT2 32
#define OFF_SID 40
#define SENTINEL_BYTES 48 /* how much of the stack top the parent pre-writes */

#define MAGIC2 0x1234abcd5678ef90ULL

struct arg {
    unsigned long magic;
    unsigned long sid;   /* spawner index this block belongs to */
    unsigned long gen;   /* globally unique spawn counter */
    unsigned long *fresh; /* freshly mmap'd page the parent wrote just before cloning */
    unsigned long pad[4];
};

/* Static pool: lets the child range-check the pointer it was handed WITHOUT
 * dereferencing it, so a stale pointer is a report and not a SIGSEGV. */
static struct arg pool[MAX_SPAWNERS * POOL_PER_SPAWNER] __attribute__((aligned(64)));

/* Published by each spawner before it clones, cleared after the child exits.
 * While a child is alive this is exactly its own `gen` — the spawner does not
 * advance until it has joined. */
static _Atomic unsigned long cur_gen[MAX_SPAWNERS];

/* Failure counters, written by children (no libc, no TLS in that context). */
static _Atomic unsigned long n_ran;
static _Atomic unsigned long bad_ptr;       /* arg pointer not in the pool */
static _Atomic unsigned long bad_magic;     /* pool entry contents stale/garbage */
static _Atomic unsigned long bad_sid;       /* spawner id read off the stack is garbage */
static _Atomic unsigned long stale_stack;   /* stack sentinels belong to an older spawn */
static _Atomic unsigned long stale_arg;     /* arg block belongs to an older spawn */
static _Atomic unsigned long stale_fresh;   /* freshly-mmap'd page reads stale/zero */

/* First divergence, captured for the report. */
static _Atomic unsigned long first_kind;
static _Atomic unsigned long first_got, first_want, first_ptr;

static void note(unsigned long kind, unsigned long got, unsigned long want, unsigned long ptr)
{
    unsigned long expect = 0;
    if (atomic_compare_exchange_strong(&first_kind, &expect, kind)) {
        atomic_store(&first_got, got);
        atomic_store(&first_want, want);
        atomic_store(&first_ptr, ptr);
    }
}

/*
 * The child's entry point. Reached with:
 *   x0 = the argument popped off the child stack
 *   x1 = stack word at SP-24, x2 = SP-32, x3 = SP-40  (parent-written sentinels)
 *
 * Runs with no valid libc TLS of its own, so: no printf, no malloc, no errno.
 * Atomics on globals and plain loads only.
 */
__attribute__((used)) static void child_fn(void *argp, unsigned long s1, unsigned long s2,
                                           unsigned long sid)
{
    atomic_fetch_add(&n_ran, 1);

    if (sid >= MAX_SPAWNERS) {
        atomic_fetch_add(&bad_sid, 1);
        note(3, sid, MAX_SPAWNERS, (unsigned long)argp);
        return;
    }

    /* The generation this spawn is supposed to be. The spawner is blocked
     * waiting for us, so this cannot advance underneath the check. */
    unsigned long want = atomic_load(&cur_gen[sid]);

    if (s1 != (SENT1 ^ want) || s2 != (SENT2 ^ want)) {
        atomic_fetch_add(&stale_stack, 1);
        note(4, s1, SENT1 ^ want, (unsigned long)argp);
        return;
    }

    uintptr_t lo = (uintptr_t)&pool[0];
    uintptr_t hi = (uintptr_t)&pool[MAX_SPAWNERS * POOL_PER_SPAWNER];
    uintptr_t p = (uintptr_t)argp;
    if (p < lo || p >= hi || (p - lo) % sizeof(struct arg) != 0) {
        atomic_fetch_add(&bad_ptr, 1);
        note(1, p, lo, p);
        return;
    }

    const struct arg *a = argp;
    if (a->magic != MAGIC) {
        atomic_fetch_add(&bad_magic, 1);
        note(2, a->magic, MAGIC, p);
        return;
    }
    if (a->gen != want || a->sid != sid) {
        atomic_fetch_add(&stale_arg, 1);
        note(5, a->gen, want, p);
        return;
    }

    /* The interesting one: a page the parent mmap'd and wrote microseconds ago,
     * i.e. one that was demand-faulted into the shared address space on the
     * parent's core and is being read for the first time on ours. This is the
     * shape of Rust's thread packet, and the shape the rustc crash implicates —
     * there, the pointer arrives intact and its *contents* are wrong. */
    const unsigned long *f = a->fresh;
    if (f[0] != (MAGIC2 ^ want) || f[1] != want) {
        atomic_fetch_add(&stale_fresh, 1);
        note(6, f[0], MAGIC2 ^ want, (unsigned long)f);
        return;
    }
    /* Same page, one cache line further in and at the far end — a partially
     * visible write would show up here and not in word 0. */
    if (f[8] != (MAGIC2 ^ want) || f[503] != want) {
        atomic_fetch_add(&stale_fresh, 1);
        note(7, f[8], MAGIC2 ^ want, (unsigned long)f);
        return;
    }
}

/*
 * Raw clone, register-for-register the same shape as musl's `__clone`, so the
 * child's first two instructions are the ones the real crash dies on. Extra:
 * the child also loads the three sentinel words the parent left below the
 * fn/arg pair and passes them on, which is what separates "stale stack" from
 * "stale argument block".
 *
 *   x0=fn  x1=stack  x2=flags  x3=arg  x4=ptid  x5=tls  x6=ctid
 */
__asm__(".text\n"
        ".globl clone_probe\n"
        ".type clone_probe,%function\n"
        "clone_probe:\n"
        "	and	x1, x1, #-16\n"
        "	stp	x0, x3, [x1, #-16]!\n"
        "	mov	x0, x2\n"
        "	mov	x2, x4\n"
        "	mov	x3, x5\n"
        "	mov	x4, x6\n"
        "	mov	x8, #220\n"
        "	svc	#0\n"
        "	cbz	x0, 1f\n"
        "	ret\n"
        "1:	ldp	x9, x0, [sp], #16\n"
        "	ldur	x1, [sp, #-24]\n"
        "	ldur	x2, [sp, #-32]\n"
        "	ldur	x3, [sp, #-40]\n"
        "	blr	x9\n"
        "	mov	x8, #93\n"
        "	svc	#0\n"
        ".size clone_probe,.-clone_probe\n");

extern long clone_probe(void (*fn)(void *, unsigned long, unsigned long, unsigned long), void *stack,
                        unsigned long flags, void *arg, int *ptid, void *tls, int *ctid);

#define FUTEX_WAIT_OP 0
#define FUTEX_PRIVATE 128

#define CLONE_VM 0x00000100
#define CLONE_FS 0x00000200
#define CLONE_FILES 0x00000400
#define CLONE_SIGHAND 0x00000800
#define CLONE_THREAD 0x00010000
#define CLONE_SETTLS 0x00080000
#define CLONE_CHILD_CLEARTID 0x00200000

static unsigned long now_us(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (unsigned long)ts.tv_sec * 1000000UL + (unsigned long)ts.tv_nsec / 1000UL;
}

static _Atomic unsigned long gen_counter = 1;
static _Atomic unsigned long n_spawn_fail;
static _Atomic unsigned long n_never_exited;
static unsigned long iters = 4000;

struct spawner_cfg {
    unsigned long sid;
};

static void *spawner(void *v)
{
    struct spawner_cfg *cfg = v;
    unsigned long sid = cfg->sid;
    void *tp = __builtin_thread_pointer();

    for (unsigned long i = 0; i < iters; i++) {
        /* Fresh stack every iteration; munmap'd at the end so the SAME virtual
         * address comes back next time round (Akuma has no ASLR). Recycling the
         * VA is the point — a stale TLB entry or a lost store shows up as the
         * child reading the *previous* iteration's words. */
        /* musl's pthread_create shape: reserve the whole mapping PROT_NONE, then
         * mprotect everything above the guard page to RW. The failing rustc
         * spawns show exactly this pair immediately before the crash:
         *   [mmap] len=0x806000 prot=0x0 ... (lazy, N regions)
         *   [mprotect] addr=+0x2000 len=0x804000 prot=0x3
         * A plain PROT_READ|PROT_WRITE mmap does NOT exercise the same kernel
         * path (PROT_NONE reservation + lazy-region flag update). */
        char *map = mmap(NULL, GUARD_SIZE + STACK_SIZE, PROT_NONE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (map == MAP_FAILED) {
            atomic_fetch_add(&n_spawn_fail, 1);
            break;
        }
        if (mprotect(map + GUARD_SIZE, STACK_SIZE, PROT_READ | PROT_WRITE) != 0) {
            atomic_fetch_add(&n_spawn_fail, 1);
            munmap(map, GUARD_SIZE + STACK_SIZE);
            break;
        }
        char *stack = map + GUARD_SIZE;
        char *top = stack + STACK_SIZE;

        unsigned long gen = atomic_fetch_add(&gen_counter, 1);

        /* A fresh anonymous page, written only by this thread, only just now:
         * the demand fault happens on THIS core and the first read happens on
         * whichever core runs the child. */
        unsigned long *fmap = mmap(NULL, 4096 * 2, PROT_NONE,
                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (fmap == MAP_FAILED || mprotect((char *)fmap + 4096, 4096,
                                           PROT_READ | PROT_WRITE) != 0) {
            atomic_fetch_add(&n_spawn_fail, 1);
            if (fmap != MAP_FAILED)
                munmap(fmap, 4096 * 2);
            munmap(map, GUARD_SIZE + STACK_SIZE);
            break;
        }
        unsigned long *fresh = (unsigned long *)((char *)fmap + 4096);
        fresh[0] = MAGIC2 ^ gen;
        fresh[1] = gen;
        fresh[8] = MAGIC2 ^ gen;
        fresh[503] = gen;

        struct arg *a = &pool[sid * POOL_PER_SPAWNER + (i % POOL_PER_SPAWNER)];
        a->magic = MAGIC;
        a->sid = sid;
        a->gen = gen;
        a->fresh = fresh;

        /* Parent-written words the child reads straight off its own stack. */
        memset(top - SENTINEL_BYTES, 0, SENTINEL_BYTES);
        *(unsigned long *)(top - OFF_SENT1) = SENT1 ^ gen;
        *(unsigned long *)(top - OFF_SENT2) = SENT2 ^ gen;
        *(unsigned long *)(top - OFF_SID) = sid;

        /* Publish before the clone: the child compares against this. */
        atomic_store(&cur_gen[sid], gen);

        volatile int ctid = -1;
        long rc = clone_probe(child_fn, top,
                              CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
                                  CLONE_SETTLS | CLONE_CHILD_CLEARTID,
                              a, NULL, tp, (int *)&ctid);
        if (rc < 0) {
            atomic_fetch_add(&n_spawn_fail, 1);
            munmap(fmap, 4096 * 2);
            munmap(map, GUARD_SIZE + STACK_SIZE);
            continue;
        }

        /* Join via CLONE_CHILD_CLEARTID: the kernel zeroes ctid and wakes on
         * thread exit. Bounded, because a child that died is exactly the
         * failure we are hunting and must not hang the probe. */
        unsigned long deadline = now_us() + 3000000UL;
        int exited = 0;
        while (now_us() < deadline) {
            int v = __atomic_load_n((int *)&ctid, __ATOMIC_ACQUIRE);
            if (v == 0) {
                exited = 1;
                break;
            }
            struct timespec to = {0, 2000000};
            syscall(SYS_futex, (int *)&ctid, FUTEX_WAIT_OP | FUTEX_PRIVATE, v, &to, NULL, 0);
        }
        if (exited) {
            munmap(fmap, 4096 * 2);
            munmap(map, GUARD_SIZE + STACK_SIZE);
        } else {
            /* Leak the stack rather than pull it out from under a live thread. */
            atomic_fetch_add(&n_never_exited, 1);
        }
    }
    return NULL;
}

static const char *kind_name(unsigned long k)
{
    switch (k) {
    case 1:
        return "arg pointer not in the static pool (child stack held a stale pointer)";
    case 2:
        return "arg block magic wrong (pointer resolved to non-argument memory)";
    case 3:
        return "spawner id read off the child stack is garbage";
    case 4:
        return "stack sentinels belong to an older spawn (child stack read is stale)";
    case 5:
        return "arg block belongs to an older spawn";
    case 6:
        return "freshly mmap'd page reads stale/zero at word 0 (parent's write not visible)";
    case 7:
        return "freshly mmap'd page reads stale/zero later in the page";
    default:
        return "?";
    }
}

int main(int argc, char **argv)
{
    unsigned long nspawn = 4;
    if (argc > 1)
        iters = strtoul(argv[1], NULL, 0);
    if (argc > 2)
        nspawn = strtoul(argv[2], NULL, 0);
    if (nspawn > MAX_SPAWNERS)
        nspawn = MAX_SPAWNERS;

    printf("clonearg: %lu spawners x %lu clone/join iterations, %d KB stacks (recycled VA)\n",
           nspawn, iters, STACK_SIZE / 1024);
    fflush(stdout);

    pthread_t th[MAX_SPAWNERS];
    struct spawner_cfg cfg[MAX_SPAWNERS];
    unsigned long t0 = now_us();
    for (unsigned long k = 0; k < nspawn; k++) {
        cfg[k].sid = k;
        if (pthread_create(&th[k], NULL, spawner, &cfg[k]) != 0) {
            printf("  pthread_create(spawner %lu) failed: %s\n", k, strerror(errno));
            nspawn = k;
            break;
        }
    }
    for (unsigned long k = 0; k < nspawn; k++)
        pthread_join(th[k], NULL);
    unsigned long dt = now_us() - t0;

    unsigned long ran = atomic_load(&n_ran);
    unsigned long div = atomic_load(&bad_ptr) + atomic_load(&bad_magic) + atomic_load(&bad_sid) +
                        atomic_load(&stale_stack) + atomic_load(&stale_arg) +
                        atomic_load(&stale_fresh);

    printf("  children ran            : %lu\n", ran);
    printf("  clone failures          : %lu\n", atomic_load(&n_spawn_fail));
    printf("  children that never exited: %lu\n", atomic_load(&n_never_exited));
    printf("  stale child stack       : %lu\n", atomic_load(&stale_stack));
    printf("  stale arg pointer       : %lu\n", atomic_load(&bad_ptr));
    printf("  arg magic wrong         : %lu\n", atomic_load(&bad_magic));
    printf("  arg block stale         : %lu\n", atomic_load(&stale_arg));
    printf("  fresh page stale        : %lu\n", atomic_load(&stale_fresh));
    printf("  garbage spawner id      : %lu\n", atomic_load(&bad_sid));
    printf("  elapsed                 : %lu ms\n", dt / 1000);

    unsigned long k = atomic_load(&first_kind);
    if (k) {
        printf("  FIRST DIVERGENCE: %s\n", kind_name(k));
        printf("    got=0x%lx want=0x%lx argptr=0x%lx\n", atomic_load(&first_got),
               atomic_load(&first_want), atomic_load(&first_ptr));
    }

    /* A child that never exited is a hang, not a data divergence, but it means
     * the same thing here: the spawn did not complete correctly. */
    unsigned long lost = atomic_load(&n_never_exited);
    printf("=== CLONEARG DONE — %lu divergence(s), %lu lost child(ren) ===\n", div, lost);
    return (div || lost) ? 1 : 0;
}
