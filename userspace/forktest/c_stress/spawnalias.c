/*
 * spawnalias — does a freshly-spawned thread see its own address space, and its
 * own clone argument?
 *
 * Written for the thread-spawn SIGSEGV class in
 * docs/runbooks/debug-thread-spawn-segv.md, and specifically to separate its
 * three live theories (§3c) from userspace, without a 30-minute rustc build:
 *
 *   T1 cross-address-space aliasing — the pointer is right, the address space
 *      is wrong, so a slot reads back as *another process's* data.
 *   T2 packet use-after-free — the argument was delivered intact and the memory
 *      behind it has already been recycled.
 *   T3 stale stack page — the child's stack VA does not resolve to the page the
 *      parent wrote.
 *
 * Why the existing probes miss it, and what this one does differently
 * -------------------------------------------------------------------
 * `clonearg` proved the *handoff* is sound (144,260 children, 0 divergences)
 * by checking values the parent wrote right before the clone. Under T1 that
 * probe passes no matter what: the handoff is not what is broken. `threadmax`
 * and `futextest` spawn threads but never ask *whose* memory the new thread is
 * looking at. So the new idea here is an *address-space identity canary*:
 *
 *   Every process fills a 256 KiB canary region with `nonce(pid) ^ page_index`,
 *   one word per page, plus copies in .data, in malloc'd heap and in a separate
 *   mmap. Every freshly-spawned thread reads all of them before doing anything
 *   else. Those live in different pages, so a *partial* aliasing event — one
 *   page or one cached translation bleeding in from another address space —
 *   makes exactly that location disagree with the others. And because the nonce
 *   is a pure function of the pid, the wrong value **names the process it came
 *   from**: `saw nonce of pid 431`.
 *
 * That is the whole point. A crash tells you a pointer was bad; this tells you
 * whose memory you were reading. Note the honest limit: if an entire address
 * space were swapped, every location would agree with every other and no
 * in-process check could see it. Partial aliasing is what T1 predicts and what
 * this catches.
 *
 * The other two theories get their own detectors:
 *
 *   T2 — the thread argument is a malloc'd packet whose first word is read
 *        exactly the way Rust's `thread_start` reads it (`ldr x20,[x0]`, then an
 *        atomic fetch-add at the loaded value). Between `pthread_create` and
 *        `pthread_join` the parent deliberately **poisons the heap** with
 *        recognisable ASCII — ANSI SGR escapes and target-feature strings, the
 *        exact content the real faults kept decoding to. So a packet that was
 *        freed does not come back as silent garbage: it comes back as text this
 *        program printf's, and the reuse is self-identifying.
 *   T3 — with `--ownstack`, threads run on a caller-supplied mmap'd stack that
 *        the parent pre-fills with a sentinel pattern. The child verifies the
 *        sentinel below its own frame. (Default off: musl's own stack
 *        allocation — mmap PROT_NONE, then mprotect — is the shape the real
 *        failure has, and it leaves no room for a caller sentinel.)
 *
 * Load shape
 * ----------
 * The class needs concurrency to appear (~1 fault per 2-4 min of `-j4`), so the
 * default run is a fan of worker *processes*, each cycling threads, with
 * short-lived `posix_spawn` children churning address spaces underneath —
 * musl implements posix_spawn with CLONE_VM|CLONE_VFORK, i.e. Akuma's vfork
 * fastpath, which is where T1's prime suspect lives (`sys_execve` calling
 * `vfork_complete` before `address_space.activate()`).
 *
 * A SIGSEGV handler is installed with SA_ONSTACK and **no** per-thread
 * sigaltstack, on purpose: that is what Rust's runtime does, and it is what
 * produces the `sig 11 needs sigaltstack but slot N has none — re-pending` line
 * that heads every report of this bug. Keeping the kernel path identical
 * matters more than catching the signal.
 *
 * Usage: spawnalias [rounds] [workers] [threads-per-round] [flags...]
 *   --ownstack        caller-allocated thread stacks with sentinels (T3)
 *   --nospawn         no posix_spawn churn (isolates threads from AS churn)
 *   --fanout          spawn all threads before joining any (thread-slot pressure)
 *   --mapfile PATH    every worker mmaps PATH — demand-paging pressure; point it
 *                     at something large, e.g. /usr/local/bin/rustc
 *   --child           internal: the short-lived posix_spawn victim
 *
 * Defaults (3000 rounds, 4 workers, 8 threads) are ~10 minutes at SMP=4.
 *
 * CALIBRATE ON LINUX before believing a failure — same rule as `futexops`:
 *   docker run --rm --platform linux/arm64 -v "$PWD/spawnalias:/spawnalias:ro" \
 *       alpine /spawnalias 300 4 8
 * Any divergence there means the probe is wrong, not the kernel.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

/* ---------------------------------------------------------------- constants */

#define PKT_MAGIC        0x5041434B45543031ull   /* "PACKET01" */
#define CANARY_PAGES     64
#define PAGE_SZ          4096u
#define CANARY_BYTES     ((size_t)CANARY_PAGES * PAGE_SZ)
#define STACK_SENTINEL   0x5354414B43414E41ull   /* "STAKCANA" */
#define OWN_STACK_BYTES  (256u * 1024u)
#define POISON_BLOCKS    64

/* Nonce is a pure function of the pid, so a wrong value names its owner. */
static uint64_t nonce_for(long pid)
{
    return 0x9E3779B97F4A7C15ull * (uint64_t)(pid + 1) + 0x1234567689ABCDEFull;
}

/* Recover the pid from a nonce we did not expect — this is what turns
 * "corrupt" into "that is pid 431's memory". */
static long pid_of_nonce(uint64_t n)
{
    /* 0x9E37... is odd, so it is invertible mod 2^64. */
    static const uint64_t inv = 0xF1DE83E19937733Dull;  /* inverse of 0x9E3779B97F4A7C15 */
    uint64_t p = (n - 0x1234567689ABCDEFull) * inv;
    return (long)p - 1;
}

/* ------------------------------------------------------------------ globals */

struct packet {
    uint64_t magic;      /* read first, the `ldr x20,[x0]` analogue */
    uint64_t refcnt;     /* target of the fetch-add that faults in the real bug */
    uint64_t nonce;      /* the creating process's AS nonce */
    uint64_t round;
    uint64_t stack_lo;   /* --ownstack: sentinel location, else 0 */
    char     tag[24];
};

static volatile uint64_t g_nonce_data;          /* .data copy */
static volatile uint64_t *g_nonce_heap;         /* malloc copy */
static volatile uint64_t *g_nonce_mmap;         /* private-mmap copy */
static volatile uint64_t *g_canary;             /* CANARY_PAGES words, 1/page */
static uint64_t g_nonce;
static long g_pid;

static atomic_ulong g_divergences;
static atomic_ulong g_threads_run;

/* --------------------------------------------------------------- reporting */

static void dump_bytes(const char *what, const void *p, size_t n)
{
    const unsigned char *b = p;
    char hex[3 * 32 + 1], asc[32 + 1];
    size_t i;
    if (n > 32) n = 32;
    for (i = 0; i < n; i++) {
        snprintf(hex + 3 * i, 4, "%02x ", b[i]);
        asc[i] = (b[i] >= 0x20 && b[i] < 0x7f) ? (char)b[i] : '.';
    }
    asc[n] = 0;
    printf("[spawnalias]   %s: %s | \"%s\"\n", what, hex, asc);
}

/* An unexpected nonce is the headline result: say whose it is. */
static void report_nonce(const char *where, uint64_t got, unsigned round)
{
    long owner = pid_of_nonce(got);
    atomic_fetch_add(&g_divergences, 1);
    printf("[spawnalias] FAIL pid=%ld round=%u %s: expected nonce %016llx, got %016llx\n",
           g_pid, round, where,
           (unsigned long long)g_nonce, (unsigned long long)got);
    if (owner > 0 && owner < 100000 && nonce_for(owner) == got) {
        printf("[spawnalias]   *** that is pid %ld's nonce — CROSS-ADDRESS-SPACE ALIASING ***\n",
               owner);
    } else {
        printf("[spawnalias]   (not any pid's nonce — corruption, not aliasing)\n");
    }
    dump_bytes("raw", &got, 8);
    fflush(stdout);
}

static void report_packet(const struct packet *pkt, uint64_t head, unsigned round)
{
    atomic_fetch_add(&g_divergences, 1);
    printf("[spawnalias] FAIL pid=%ld round=%u packet %p: first word %016llx, expected %016llx\n",
           g_pid, round, (const void *)pkt,
           (unsigned long long)head, (unsigned long long)PKT_MAGIC);
    dump_bytes("packet", pkt, 32);
    fflush(stdout);
}

/* ------------------------------------------------------- the probe itself */

/* Runs as the first thing a brand-new thread does, deliberately before any
 * libc call that might fault something in and mask the state we are sampling. */
static void *probe_thread(void *arg)
{
    struct packet *pkt = arg;

    /* T2: the exact shape of the faulting prologue — load the first word, then
     * atomically bump a counter at what it describes. */
    uint64_t head = pkt->magic;
    if (head != PKT_MAGIC) {
        report_packet(pkt, head, 0);
    } else {
        atomic_fetch_add((atomic_ullong *)&pkt->refcnt, 1);
    }

    /* T1: four independent pages, all of which must agree. */
    uint64_t expect = pkt->nonce;
    if (g_nonce_data != expect)   report_nonce("data-segment canary", g_nonce_data, (unsigned)pkt->round);
    if (*g_nonce_heap != expect)  report_nonce("heap canary", *g_nonce_heap, (unsigned)pkt->round);
    if (*g_nonce_mmap != expect)  report_nonce("mmap canary", *g_nonce_mmap, (unsigned)pkt->round);

    for (int i = 0; i < CANARY_PAGES; i++) {
        uint64_t want = expect ^ (uint64_t)i;
        uint64_t got = g_canary[(size_t)i * (PAGE_SZ / sizeof(uint64_t))];
        if (got != want) {
            char where[64];
            snprintf(where, sizeof where, "canary page %d", i);
            report_nonce(where, got ^ (uint64_t)i, (unsigned)pkt->round);
            break;   /* one report per thread is enough; the rest would be noise */
        }
    }

    /* T3: the parent's sentinel, below this thread's own frame. */
    if (pkt->stack_lo) {
        const volatile uint64_t *s = (const volatile uint64_t *)(uintptr_t)pkt->stack_lo;
        if (*s != STACK_SENTINEL) {
            atomic_fetch_add(&g_divergences, 1);
            printf("[spawnalias] FAIL pid=%ld round=%llu stack sentinel at %p: %016llx != %016llx\n",
                   g_pid, (unsigned long long)pkt->round, (void *)(uintptr_t)pkt->stack_lo,
                   (unsigned long long)*s, (unsigned long long)STACK_SENTINEL);
            fflush(stdout);
        }
    }

    atomic_fetch_add(&g_threads_run, 1);
    return NULL;
}

/* --------------------------------------------------------------- the load */

/* Recycle freed memory into content that is recognisable in a fault dump. The
 * real faults kept decoding to exactly this: ANSI SGR escapes from rustc's
 * colorised output, and target-feature strings. If a packet is freed while its
 * thread is starting, this is what the thread will read. */
static void poison_heap(void)
{
    static const char *const shapes[] = {
        "\x1b[1m\x1b[92m\x1b[0m\x1b[1m",
        "+strict-align,+outline-atomics",
        "libder-8e2204f797eee486.rmeta",
    };
    void *blocks[POISON_BLOCKS];
    for (int i = 0; i < POISON_BLOCKS; i++) {
        size_t n = sizeof(struct packet) + (size_t)(i % 3) * 8;
        blocks[i] = malloc(n);
        if (blocks[i]) {
            const char *s = shapes[i % 3];
            size_t l = strlen(s);
            for (size_t o = 0; o < n; o += l) {
                memcpy((char *)blocks[i] + o, s, (n - o < l) ? n - o : l);
            }
        }
    }
    for (int i = 0; i < POISON_BLOCKS; i++) free(blocks[i]);
}

struct opts {
    unsigned rounds, workers, threads;
    int ownstack, nospawn, fanout;
    const char *mapfile;
};

static int spawn_one_thread(pthread_t *out, struct packet *pkt, const struct opts *o)
{
    pthread_attr_t attr;
    pthread_attr_t *ap = NULL;
    void *stack = NULL;

    pkt->magic = PKT_MAGIC;
    pkt->refcnt = 0;
    pkt->nonce = g_nonce;
    pkt->stack_lo = 0;
    memcpy(pkt->tag, "spawnalias-packet", 18);

    if (o->ownstack) {
        stack = mmap(NULL, OWN_STACK_BYTES, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (stack == MAP_FAILED) return -1;
        /* Fill the whole region, then hand musl the top. The child checks a word
         * near the base, which musl's thread struct never reaches. */
        uint64_t *w = stack;
        for (size_t i = 0; i < OWN_STACK_BYTES / sizeof(uint64_t); i++) w[i] = STACK_SENTINEL;
        pkt->stack_lo = (uint64_t)(uintptr_t)stack + 64;
        pthread_attr_init(&attr);
        pthread_attr_setstack(&attr, stack, OWN_STACK_BYTES);
        ap = &attr;
    }

    int rc = pthread_create(out, ap, probe_thread, pkt);
    if (ap) pthread_attr_destroy(ap);
    if (rc != 0 && stack) munmap(stack, OWN_STACK_BYTES);
    return rc;
}

static void churn_address_space(const char *self)
{
    /* posix_spawn == CLONE_VM|CLONE_VFORK in musl == Akuma's vfork fastpath.
     * The child exits immediately, so address spaces are being built and torn
     * down while this process is starting threads — T1's amplifier. */
    char *const argv[] = { (char *)self, (char *)"--child", NULL };
    pid_t pid;
    if (posix_spawn(&pid, self, NULL, NULL, argv, environ) == 0) {
        int st;
        waitpid(pid, &st, 0);
    }
}

static int run_worker(const struct opts *o, const char *self)
{
    pthread_t *tids = calloc(o->threads, sizeof *tids);
    struct packet **pkts = calloc(o->threads, sizeof *pkts);
    if (!tids || !pkts) return 1;

    for (unsigned r = 0; r < o->rounds; r++) {
        if (o->fanout) {
            for (unsigned t = 0; t < o->threads; t++) {
                pkts[t] = malloc(sizeof **pkts);
                if (!pkts[t]) continue;
                pkts[t]->round = r;
                if (spawn_one_thread(&tids[t], pkts[t], o) != 0) { free(pkts[t]); pkts[t] = NULL; }
            }
            poison_heap();
            for (unsigned t = 0; t < o->threads; t++) {
                if (pkts[t]) { pthread_join(tids[t], NULL); free(pkts[t]); pkts[t] = NULL; }
            }
        } else {
            for (unsigned t = 0; t < o->threads; t++) {
                struct packet *p = malloc(sizeof *p);
                if (!p) continue;
                p->round = r;
                pthread_t tid;
                if (spawn_one_thread(&tid, p, o) != 0) { free(p); continue; }
                /* The window that matters: the packet is live, its thread is
                 * starting, and the heap around it is being recycled. */
                poison_heap();
                pthread_join(tid, NULL);
                free(p);
            }
        }

        if (!o->nospawn && (r % 8) == 0) churn_address_space(self);

        if ((r % 250) == 0 && r) {
            printf("[spawnalias] pid=%ld round %u/%u threads=%lu divergences=%lu\n",
                   g_pid, r, o->rounds,
                   (unsigned long)atomic_load(&g_threads_run),
                   (unsigned long)atomic_load(&g_divergences));
            fflush(stdout);
        }
    }
    free(tids);
    free(pkts);
    return atomic_load(&g_divergences) != 0;
}

/* ------------------------------------------------------------------- setup */

static void segv_handler(int sig, siginfo_t *si, void *uc)
{
    (void)uc;
    /* Best-effort: the interesting case is when this never runs because the
     * thread had no altstack and the kernel killed the group instead. */
    char buf[128];
    int n = snprintf(buf, sizeof buf,
                     "[spawnalias] SIG%d at %p pid=%ld\n", sig, si->si_addr, g_pid);
    ssize_t ignored = write(1, buf, (size_t)n);
    (void)ignored;
    _exit(3);
}

static int init_process_identity(const char *mapfile)
{
    g_pid = (long)getpid();
    g_nonce = nonce_for(g_pid);

    g_nonce_data = g_nonce;

    g_nonce_heap = malloc(sizeof(uint64_t));
    if (!g_nonce_heap) return -1;
    *g_nonce_heap = g_nonce;

    void *m = mmap(NULL, PAGE_SZ, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) return -1;
    g_nonce_mmap = m;
    *g_nonce_mmap = g_nonce;

    void *c = mmap(NULL, CANARY_BYTES, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (c == MAP_FAILED) return -1;
    g_canary = c;
    for (int i = 0; i < CANARY_PAGES; i++) {
        g_canary[(size_t)i * (PAGE_SZ / sizeof(uint64_t))] = g_nonce ^ (uint64_t)i;
    }

    if (mapfile) {
        /* Demand-paging pressure, the librustc_driver stand-in. Touch one word
         * per page so the mapping is really faulted in, then leave it mapped. */
        FILE *f = fopen(mapfile, "rb");
        if (f) {
            fseek(f, 0, SEEK_END);
            long len = ftell(f);
            fclose(f);
            if (len > 0) {
                int fd = open(mapfile, O_RDONLY);
                if (fd >= 0) {
                    void *fm = mmap(NULL, (size_t)len, PROT_READ, MAP_PRIVATE, fd, 0);
                    close(fd);
                    if (fm != MAP_FAILED) {
                        volatile unsigned char *p = fm;
                        unsigned long sum = 0;
                        for (long off = 0; off < len; off += PAGE_SZ) sum += p[off];
                        (void)sum;
                    }
                }
            }
        }
    }
    return 0;
}

int main(int argc, char **argv)
{
    struct opts o = { 3000, 4, 8, 0, 0, 0, NULL };
    int positional = 0;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--child")) return 0;          /* the spawn victim */
        else if (!strcmp(argv[i], "--ownstack")) o.ownstack = 1;
        else if (!strcmp(argv[i], "--nospawn")) o.nospawn = 1;
        else if (!strcmp(argv[i], "--fanout")) o.fanout = 1;
        else if (!strcmp(argv[i], "--mapfile") && i + 1 < argc) o.mapfile = argv[++i];
        else if (argv[i][0] != '-') {
            unsigned v = (unsigned)strtoul(argv[i], NULL, 10);
            /* Positional by count, not by "still at its default" — passing the
             * default value for an argument must not shift the ones after it. */
            switch (positional++) {
                case 0: o.rounds = v; break;
                case 1: o.workers = v; break;
                default: o.threads = v; break;
            }
        }
    }
    if (o.workers == 0) o.workers = 1;
    if (o.threads == 0) o.threads = 1;

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = segv_handler;
    /* SA_ONSTACK with no per-thread altstack, exactly like the Rust runtime —
     * this is what makes the kernel print "needs sigaltstack ... re-pending". */
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGSEGV, &sa, NULL);

    printf("[spawnalias] rounds=%u workers=%u threads=%u ownstack=%d spawn=%d fanout=%d map=%s\n",
           o.rounds, o.workers, o.threads, o.ownstack, !o.nospawn, o.fanout,
           o.mapfile ? o.mapfile : "(none)");
    fflush(stdout);

    pid_t kids[64];
    unsigned nk = o.workers > 64 ? 64 : o.workers;
    for (unsigned i = 0; i < nk; i++) {
        pid_t p = fork();
        if (p == 0) {
            if (init_process_identity(o.mapfile) != 0) _exit(2);
            _exit(run_worker(&o, argv[0]));
        }
        kids[i] = p;
    }

    int bad = 0;
    for (unsigned i = 0; i < nk; i++) {
        int st = 0;
        if (kids[i] > 0 && waitpid(kids[i], &st, 0) > 0) {
            if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
                printf("[spawnalias] worker %u (pid %d) exited abnormally: status=0x%x\n",
                       i, (int)kids[i], st);
                bad = 1;
            }
        }
    }

    printf("[spawnalias] %s\n", bad ? "FAIL — see divergences above" : "PASS — no divergence");
    return bad;
}
