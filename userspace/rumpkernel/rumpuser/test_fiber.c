/*
 * test_fiber.c — standalone exercise of the fiber rumpuser backend
 * (src/fiber.rs, built with --features threads_fiber), with NO rump kernel.
 *
 * We stub the two hyp scheduler upcalls (backend_unschedule/schedule) that the
 * fiber sync paths call via rumpkern_{un,}sched, then drive the cooperative
 * scheduler directly through the rumpuser_* hypercalls:
 *   Test A: create joinable fibers that cooperatively clock_sleep + exit; join.
 *   Test B: two fibers ping-pong N rounds over a mutex + condvar.
 *
 * PASS = deterministic interleavings below + "ALL TESTS PASSED".
 * Built static for aarch64-linux-musl; runs in arm64 Linux and Akuma EL0.
 */
#include <stdio.h>
#include <stdint.h>

/* RumpHyperUp layout (src/lib.rs): 13 fn ptrs + hyp_extra[8] = 21 pointers.
 * backend_unschedule is index 2, backend_schedule index 3. */
struct rumphyperup {
	void *p[21];
};

/* rumpuser_* ABI we drive */
extern int  rumpuser_init(int version, const struct rumphyperup *hyp);
extern int  rumpuser_thread_create(void *(*f)(void *), void *arg, const char *name,
                                   int mustjoin, int pri, int cpuidx, void **cookie);
extern void rumpuser_thread_exit(void) __attribute__((noreturn));
extern int  rumpuser_thread_join(void *cookie);
extern int  rumpuser_curlwpop(int op, void *lwp);
extern int  rumpuser_clock_sleep(int enum_, int64_t sec, long nsec);
extern void rumpuser_mutex_init(void **mtxp, int flags);
extern void rumpuser_mutex_enter(void *m);
extern void rumpuser_mutex_exit(void *m);
extern void rumpuser_cv_init(void **cvp);
extern void rumpuser_cv_wait(void *cv, void *m);
extern void rumpuser_cv_signal(void *cv);

#define RUMPUSER_LWP_SET     2
#define RUMPUSER_CLOCK_RELWALL 0

/* The Rust staticlib (panic=abort) still emits a reference to this personality
 * symbol; stub it for the standalone C link. (The real rump_server link pulls it
 * in via its own glue.) */
void rust_eh_personality(void) {}

/* no-op scheduler upcalls (no real rump CPU here) */
static void be_unsched(int a, int *nlocks, void *il) { (void)a; (void)il; if (nlocks) *nlocks = 0; }
static void be_sched(int nlocks, void *il) { (void)nlocks; (void)il; }

/* ── Test A: scheduler + clock_sleep + exit + join ── */
static void *
sleeper(void *arg)
{
	int id = (int)(intptr_t)arg;
	rumpuser_curlwpop(RUMPUSER_LWP_SET, (void *)(intptr_t)(id + 1));
	printf("[A%d] start\n", id);
	fflush(stdout);
	rumpuser_clock_sleep(RUMPUSER_CLOCK_RELWALL, 0, (long)(id + 1) * 20 * 1000 * 1000); /* (id+1)*20ms */
	printf("[A%d] woke\n", id);
	fflush(stdout);
	rumpuser_thread_exit();
}

/* ── Test B: mutex + condvar ping-pong ── */
static void *cv_mtx;
static void *cv_cv;
static int cv_turn;     /* whose turn: 0 or 1 */
static int cv_rounds;   /* completed lines */
#define ROUNDS 5

static void *
pinger(void *arg)
{
	int me = (int)(intptr_t)arg; /* 0 or 1 */
	rumpuser_curlwpop(RUMPUSER_LWP_SET, (void *)(intptr_t)(100 + me));
	for (int r = 0; r < ROUNDS; r++) {
		rumpuser_mutex_enter(cv_mtx);
		while (cv_turn != me)
			rumpuser_cv_wait(cv_cv, cv_mtx);
		printf("[B%c] round %d\n", me ? 'Q' : 'P', r);
		fflush(stdout);
		cv_rounds++;
		cv_turn = !me;
		rumpuser_cv_signal(cv_cv);
		rumpuser_mutex_exit(cv_mtx);
	}
	rumpuser_thread_exit();
}

int
main(void)
{
	struct rumphyperup hyp = {{0}};
	hyp.p[2] = (void *)be_unsched;
	hyp.p[3] = (void *)be_sched;
	if (rumpuser_init(17, &hyp) != 0) {
		printf("rumpuser_init failed\n");
		return 1;
	}
	rumpuser_curlwpop(RUMPUSER_LWP_SET, (void *)(intptr_t)999); /* main lwp */

	/* ── Test A ── */
	printf("== Test A: create / clock_sleep / join ==\n");
	fflush(stdout);
	void *ca[3];
	for (int i = 0; i < 3; i++)
		rumpuser_thread_create(sleeper, (void *)(intptr_t)i, "sleeper", 1, 0, 0, &ca[i]);
	for (int i = 0; i < 3; i++)
		rumpuser_thread_join(ca[i]);
	printf("[A] all joined\n");
	fflush(stdout);

	/* ── Test B ── */
	printf("== Test B: mutex + condvar ping-pong ==\n");
	fflush(stdout);
	rumpuser_mutex_init(&cv_mtx, 0);
	rumpuser_cv_init(&cv_cv);
	cv_turn = 0;
	cv_rounds = 0;
	void *cp, *cq;
	rumpuser_thread_create(pinger, (void *)(intptr_t)0, "P", 1, 0, 0, &cp);
	rumpuser_thread_create(pinger, (void *)(intptr_t)1, "Q", 1, 0, 0, &cq);
	rumpuser_thread_join(cp);
	rumpuser_thread_join(cq);
	printf("[B] ping-pong done, rounds=%d (expect %d)\n", cv_rounds, 2 * ROUNDS);
	fflush(stdout);

	if (cv_rounds == 2 * ROUNDS) {
		printf("ALL TESTS PASSED\n");
		return 0;
	}
	printf("FAIL\n");
	return 1;
}
