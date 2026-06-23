/*
 * sp_serve_fd.c — Akuma addition (RUMP_SYSPROXY.md Step 4, kernel-pipe transport).
 *
 * Serve the rump sysproxy protocol on a SINGLE PRE-CONNECTED fd, instead of
 * NetBSD's `rumpuser_sp_init(url)` which does socket/bind/listen/accept on a
 * URL. Akuma has no path-based AF_UNIX; the kernel hands rump_server one end of
 * a kernel pipe pair as an inherited fd, and we serve the sysproxy wire on it.
 *
 * This file #includes the NetBSD `rumpuser_sp.c` to reach its (static) per-client
 * machinery (spclist/pfdlist, readframe, handlereq, kickwaiter, banner, ...) and
 * adds `rumpuser_sp_init_fd()` + a one-client serve loop reduced from `spserver`.
 * The NetBSD source is unmodified; this addition is original Akuma code derived
 * from NetBSD's `spserver`/`serv_handleconn` (NetBSD project, BSD-licensed,
 * copyright the NetBSD contributors).
 *
 * Compile this INSTEAD of rumpuser_sp.c (the #include pulls it in).
 */
#include <pthread.h>

/*
 * Akuma fiber-backend adaptation. Under the cooperative (fiber) rumpuser, the
 * sysproxy server's threads — the receiver (spserver_fd, below) AND the per-request
 * workers (serv_workbouncer, spawned in rumpuser_sp.c:schedulework) — MUST be
 * fibers on the one OS thread. A raw pthread is a 2nd OS thread calling into the
 * lock-free fiber rump kernel → KASSERT/abort (the SIGABRT we observed). So we
 * redirect pthread_create/detach to the rumpuser thread hypercalls, deciding at
 * RUNTIME (cooperative? → fiber, else → real pthread) so the SAME compiled object
 * works against either rumpuser backend. NetBSD's rumpuser_sp.c stays textually
 * unmodified — the redirect lives entirely here, applied via the macros below
 * before the #include.
 */
extern int rumpuser_thread_create(void *(*f)(void *), void *arg, const char *thrname,
    int joinable, int pri, int cpuidx, void **cookie);
extern int rumpuser_akuma_cooperative(void);
extern void rumpuser_akuma_yield(void);

/* real libc entry points, aliased so the shim can fall back without the macro */
extern int __akuma_real_pthread_create(pthread_t *, const pthread_attr_t *,
    void *(*)(void *), void *) __asm__("pthread_create");
extern int __akuma_real_pthread_detach(pthread_t) __asm__("pthread_detach");

static int
akuma_sp_pthread_create(pthread_t *t, const pthread_attr_t *attr,
    void *(*fn)(void *), void *arg)
{
	if (rumpuser_akuma_cooperative()) {
		void *cookie = NULL;
		int rv = rumpuser_thread_create(fn, arg, "rumpsp", 0, 0, -1, &cookie);
		if (rv == 0 && t != NULL)
			*t = (pthread_t)cookie;
		return rv;
	}
	return __akuma_real_pthread_create(t, attr, fn, arg);
}
static int
akuma_sp_pthread_detach(pthread_t t)
{
	if (rumpuser_akuma_cooperative())
		return 0; /* fibers here are detached/non-joinable */
	return __akuma_real_pthread_detach(t);
}
#define pthread_create akuma_sp_pthread_create
#define pthread_detach akuma_sp_pthread_detach

/*
 * Cooperative mutex/cond redirect (fiber backend). NetBSD's rumpuser_sp.c uses
 * raw pthread_mutex_t/pthread_cond_t — notably the COPYIN `waitresp` does a
 * pthread_cond_wait. On the SINGLE OS thread of the fiber backend that real cond
 * wait blocks the OS thread (a futex) and DEADLOCKS the cooperative scheduler: the
 * worker fiber parks mid-syscall and the receiver fiber that would wake it can
 * never run (a proxied `bind` then stalls to the kernel's read timeout → the DNS
 * resolver fails). So route the locks/cvs to fiber wait-queues (akfiber_sp_* in
 * fiber.rs), deciding at RUNTIME like pthread_create above. NetBSD source stays
 * textually unmodified — the redirect lives here, applied before the #include.
 */
extern int akfiber_sp_mutex_init(void *);
extern int akfiber_sp_mutex_lock(void *);
extern int akfiber_sp_mutex_unlock(void *);
extern int akfiber_sp_cond_init(void *);
extern int akfiber_sp_cond_wait(void *, void *);
extern int akfiber_sp_cond_signal(void *);
extern int akfiber_sp_cond_broadcast(void *);
extern int akfiber_sp_cond_destroy(void *);

extern int __akuma_real_pthread_mutex_lock(pthread_mutex_t *)
    __asm__("pthread_mutex_lock");
extern int __akuma_real_pthread_mutex_unlock(pthread_mutex_t *)
    __asm__("pthread_mutex_unlock");
extern int __akuma_real_pthread_mutex_init(pthread_mutex_t *, const pthread_mutexattr_t *)
    __asm__("pthread_mutex_init");
extern int __akuma_real_pthread_cond_init(pthread_cond_t *, const pthread_condattr_t *)
    __asm__("pthread_cond_init");
extern int __akuma_real_pthread_cond_wait(pthread_cond_t *, pthread_mutex_t *)
    __asm__("pthread_cond_wait");
extern int __akuma_real_pthread_cond_signal(pthread_cond_t *)
    __asm__("pthread_cond_signal");
extern int __akuma_real_pthread_cond_broadcast(pthread_cond_t *)
    __asm__("pthread_cond_broadcast");
extern int __akuma_real_pthread_cond_destroy(pthread_cond_t *)
    __asm__("pthread_cond_destroy");

static int akuma_sp_mutex_lock(pthread_mutex_t *m) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_mutex_lock(m)
	                                    : __akuma_real_pthread_mutex_lock(m);
}
static int akuma_sp_mutex_unlock(pthread_mutex_t *m) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_mutex_unlock(m)
	                                    : __akuma_real_pthread_mutex_unlock(m);
}
static int akuma_sp_mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *a) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_mutex_init(m)
	                                    : __akuma_real_pthread_mutex_init(m, a);
}
static int akuma_sp_cond_init(pthread_cond_t *c, const pthread_condattr_t *a) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_cond_init(c)
	                                    : __akuma_real_pthread_cond_init(c, a);
}
static int akuma_sp_cond_wait(pthread_cond_t *c, pthread_mutex_t *m) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_cond_wait(c, m)
	                                    : __akuma_real_pthread_cond_wait(c, m);
}
static int akuma_sp_cond_signal(pthread_cond_t *c) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_cond_signal(c)
	                                    : __akuma_real_pthread_cond_signal(c);
}
static int akuma_sp_cond_broadcast(pthread_cond_t *c) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_cond_broadcast(c)
	                                    : __akuma_real_pthread_cond_broadcast(c);
}
static int akuma_sp_cond_destroy(pthread_cond_t *c) {
	return rumpuser_akuma_cooperative() ? akfiber_sp_cond_destroy(c)
	                                    : __akuma_real_pthread_cond_destroy(c);
}
#define pthread_mutex_lock akuma_sp_mutex_lock
#define pthread_mutex_unlock akuma_sp_mutex_unlock
#define pthread_mutex_init akuma_sp_mutex_init
#define pthread_cond_init akuma_sp_cond_init
#define pthread_cond_wait akuma_sp_cond_wait
#define pthread_cond_signal akuma_sp_cond_signal
#define pthread_cond_broadcast akuma_sp_cond_broadcast
#define pthread_cond_destroy akuma_sp_cond_destroy

#include "rumpuser_sp.c"

/*
 * One-client serve loop: poll the single pre-connected fd (seeded at slot 1, no
 * listener at slot 0) and dispatch frames exactly like spserver's client branch.
 */
static void *
spserver_fd(void *arg)
{
	struct spservarg *sarg = arg;
	int connfd = sarg->sps_sock;
	struct spclient *spc;
	unsigned idx, maxidx;
	int rv, seen, flags;
	int coop = rumpuser_akuma_cooperative();

	/* mirror spserver's slot init */
	for (idx = 0; idx < MAXCLI; idx++) {
		pfdlist[idx].fd = -1;
		pfdlist[idx].events = POLLIN;
		spc = &spclist[idx];
		pthread_mutex_init(&spc->spc_mtx, NULL);
		pthread_cond_init(&spc->spc_cv, NULL);
		spc->spc_fd = -1;
	}
	pthread_attr_init(&pattr_detached);
	pthread_attr_setdetachstate(&pattr_detached, PTHREAD_CREATE_DETACHED);
	pthread_mutex_init(&sbamtx, NULL);
	pthread_cond_init(&sbacv, NULL);

	/* seed the connected fd as the single client (slot 1); slot 0 stays inert
	 * (fd == -1 → poll ignores it, the "new connection" branch never fires). */
	flags = fcntl(connfd, F_GETFL, 0);
	(void)fcntl(connfd, F_SETFL, flags | O_NONBLOCK);
	if (send(connfd, banner, strlen(banner), MSG_NOSIGNAL) != (ssize_t)strlen(banner)) {
		fprintf(stderr, "rump_sp(fd): banner send failed\n");
		free(sarg);
		return NULL;
	}
	idx = 1;
	pfdlist[idx].fd = connfd;
	spclist[idx].spc_fd = connfd;
	spclist[idx].spc_istatus = SPCSTATUS_BUSY; /* dedicated receiver */
	spclist[idx].spc_refcnt = 1;
	TAILQ_INIT(&spclist[idx].spc_respwait);
	maxidx = 1;

	free(sarg);

	for (;;) {
		seen = 0;
		/* fiber backend: poll must NOT block the one OS thread — poll with a
		 * zero timeout and cooperatively yield when idle so the rest of the rump
		 * kernel runs. pthread backend: block in poll on this thread as before. */
		rv = poll(pfdlist, maxidx + 1, coop ? 0 : INFTIM);
		if (rv == -1) {
			if (errno == EINTR)
				continue;
			fprintf(stderr, "rump_sp(fd): poll errno %d\n", errno);
			break;
		}
		if (rv == 0) {
			if (coop)
				rumpuser_akuma_yield();
			continue;
		}
		for (idx = 0; seen < rv && idx < MAXCLI; idx++) {
			if ((pfdlist[idx].revents & POLLIN) == 0)
				continue;
			seen++;
			if (idx == 0)
				continue; /* no listener in fd mode */
			spc = &spclist[idx];
			switch (readframe(spc)) {
			case 0:
				break;
			case -1:
				serv_handledisco(idx);
				goto out; /* single client gone → done */
			default:
				switch (spc->spc_hdr.rsp_class) {
				case RUMPSP_RESP:
					kickwaiter(spc);
					break;
				case RUMPSP_REQ:
					handlereq(spc);
					break;
				default:
					send_error_resp(spc, spc->spc_hdr.rsp_reqno,
					    RUMPSP_ERR_MALFORMED_REQUEST);
					spcfreebuf(spc);
					break;
				}
				break;
			}
		}
	}
out:
	return NULL;
}

/*
 * Serve the sysproxy protocol on `connfd` (a connected stream fd). Mirrors
 * rumpuser_sp_init's banner setup + worker-thread launch, minus the listener.
 */
int
rumpuser_sp_init_fd(int connfd, const char *ostype, const char *osrelease,
	const char *machine)
{
	pthread_t pt;
	struct spservarg *sarg;
	int error;

	snprintf(banner, sizeof(banner), "RUMPSP-%d.%d-%s-%s/%s\n",
	    PROTOMAJOR, PROTOMINOR, ostype, osrelease, machine);

	sarg = malloc(sizeof(*sarg));
	if (sarg == NULL)
		return ENOMEM;
	sarg->sps_sock = connfd;
	sarg->sps_connhook = (connecthook_fn)success; /* unix-style: no-op */

	if ((error = pthread_create(&pt, NULL, spserver_fd, sarg)) != 0) {
		free(sarg);
		return error;
	}
	pthread_detach(pt);
	return 0;
}
