/*
 * rumpcomp_tap.c — Akuma's OWN virtif packet backend: rumpcomp_virt_* over the
 * kernel's raw L2 device /dev/net/tap0 (Phase 3), replacing the stock Linux
 * TUN/TAP backend (virtif_user.c). This is the backend that runs INSIDE an Akuma
 * box; the kernel side (NIC1 → /dev/net/tap0) already exists (RUMP_NIC=1).
 *
 * Differences vs. the stock/container backend:
 *  - open("/dev/net/tap0") directly; NO /dev/net/tun + TUNSETIFF (the Akuma tap is
 *    a clean packet device, not a Linux TUN/TAP impersonation).
 *  - the kernel tap fd is non-blocking with NO poll/epoll yet, so the RX thread
 *    BUSY-POLLS read() (EAGAIN → short nanosleep) instead of poll()ing.
 *
 * Same instrumentation as virtif_user_instr.c: per-frame counters/log at the
 * rump↔wire seam (RUMP_VIRTIF_TRACE=1) + virtif_dump_stats() (the proof).
 */
#ifndef _KERNEL
#include <sys/types.h>
#include <sys/uio.h>

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <rump/rumpuser_component.h>

#include "if_virt.h"
#include "virtif_user.h"

#if VIFHYPER_REVISION != 20140313
#error VIFHYPER_REVISION mismatch
#endif

#define TAPDEV "/dev/net/tap0"

/*
 * Create the RX thread via the rumpuser hypercall, NOT pthread_create directly.
 * Under the default (pthread) rumpuser this still maps to a host pthread; under
 * the cooperative (fiber) rumpuser it becomes a fiber on the single OS thread —
 * which is REQUIRED there, because rcvthread calls rumpuser_component_* /
 * VIF_DELIVERPKT into the rump kernel: a raw 2nd OS thread (pthread_create) would
 * race the fiber backend's lock-free scheduler state (wrong curlwp → KASSERT).
 * (declared here; provided by our Rust rumpuser staticlib.)
 */
extern int rumpuser_thread_create(void *(*f)(void *), void *arg, const char *thrname,
    int joinable, int pri, int cpuidx, void **cookie);
extern int rumpuser_thread_join(void *cookie);

/*
 * Backend-capability hooks (provided by our Rust rumpuser). Under the fiber
 * backend all rump kthreads share ONE OS thread, so a BLOCKING tap read would
 * freeze every fiber (the RX fiber parks the OS thread before the stack can even
 * send a DHCP DISCOVER → deadlock). There we open the tap non-blocking and yield
 * cooperatively on EAGAIN. Under pthread (cooperative==0) we keep the blocking
 * read (the RX is its own host thread; no change to the shipped M2 path).
 */
extern int rumpuser_akuma_cooperative(void);
extern void rumpuser_akuma_yield(void);

static volatile unsigned long g_tx_pkts, g_tx_bytes, g_rx_pkts, g_rx_bytes;
static int g_trace = -1;

static void
trace_init(void)
{
	if (g_trace == -1) {
		const char *e = getenv("RUMP_VIRTIF_TRACE");
		g_trace = (e && *e && *e != '0') ? 1 : 0;
	}
}

static void
log_frame(const char *dir, struct iovec *iov, size_t iovlen, unsigned long seq)
{
	size_t total = 0, i;
	unsigned char *p;
	for (i = 0; i < iovlen; i++)
		total += iov[i].iov_len;
	fprintf(stderr, "[VIRTIF %s #%lu] %zu bytes", dir, seq, total);
	if (iovlen > 0 && iov[0].iov_len >= 14) {
		p = iov[0].iov_base;
		fprintf(stderr, " dst=%02x:%02x:%02x:%02x:%02x:%02x ethtype=0x%02x%02x",
		    p[0], p[1], p[2], p[3], p[4], p[5], p[12], p[13]);
	}
	fprintf(stderr, "\n");
}

void
virtif_dump_stats(void)
{
	fprintf(stderr,
	    "[VIRTIF STATS] tx=%lu pkts/%lu bytes  rx=%lu pkts/%lu bytes "
	    "(carried by the NetBSD rump stack over /dev/net/tap0)\n",
	    g_tx_pkts, g_tx_bytes, g_rx_pkts, g_rx_bytes);
}

struct virtif_user {
	struct virtif_sc *viu_virtifsc;
	int viu_fd;
	int viu_dying;
	void *viu_rcvcookie;   /* rumpuser_thread_create cookie (pthread or fiber) */
	char viu_rcvbuf[9018];
};

static void *
rcvthread(void *aaargh)
{
	struct virtif_user *viu = aaargh;
	struct iovec iov;
	ssize_t nn;

	rumpuser_component_kthread();

	/* pthread backend: the tap fd is BLOCKING; read() parks THIS host thread until
	 * a frame arrives (no rump CPU held, so other rump threads keep running).
	 * fiber backend: the fd is non-blocking; on EAGAIN we cooperatively yield so
	 * the rest of the rump kernel (and the DHCP path) runs on the one OS thread. */
	int coop = rumpuser_akuma_cooperative();
	while (!viu->viu_dying) {
		nn = read(viu->viu_fd, viu->viu_rcvbuf, sizeof(viu->viu_rcvbuf));
		if (nn < 1) {
			if (coop)
				rumpuser_akuma_yield();
			continue;
		}
		iov.iov_base = viu->viu_rcvbuf;
		iov.iov_len = nn;

		g_rx_pkts++;
		g_rx_bytes += (unsigned long)nn;
		if (g_trace == 1)
			log_frame("RX", &iov, 1, g_rx_pkts);

		rumpuser_component_schedule(NULL);
		VIF_DELIVERPKT(viu->viu_virtifsc, &iov, 1);
		rumpuser_component_unschedule();
	}
	rumpuser_component_kthread_release();
	return NULL;
}

int
VIFHYPER_CREATE(const char *devstr, struct virtif_sc *vif_sc, uint8_t *enaddr,
	struct virtif_user **viup)
{
	struct virtif_user *viu = NULL;
	void *cookie;
	int rv;

	(void)devstr;   /* single tap device; ignore the unit string */
	trace_init();
	cookie = rumpuser_component_unschedule();

	viu = calloc(1, sizeof(*viu));
	if (viu == NULL) { rv = errno; goto err1; }
	viu->viu_virtifsc = vif_sc;

	viu->viu_fd = open(TAPDEV, O_RDWR | (rumpuser_akuma_cooperative() ? O_NONBLOCK : 0));
	if (viu->viu_fd == -1) {
		fprintf(stderr, "rumpcomp_tap: can't open %s: %s\n",
		    TAPDEV, strerror(errno));
		rv = errno;
		goto err2;
	}

	if ((rv = rumpuser_thread_create(rcvthread, viu, "tap-rx", 1, 0, -1,
	    &viu->viu_rcvcookie)) != 0)
		goto err3;

	rumpuser_component_schedule(cookie);
	*viup = viu;
	return 0;

 err3:
	close(viu->viu_fd);
 err2:
	free(viu);
 err1:
	rumpuser_component_schedule(cookie);
	return rumpuser_component_errtrans(rv);
}

void
VIFHYPER_SEND(struct virtif_user *viu, struct iovec *iov, size_t iovlen)
{
	void *cookie = rumpuser_component_unschedule();
	size_t i, total = 0;
	ssize_t idontcare __attribute__((__unused__));

	for (i = 0; i < iovlen; i++)
		total += iov[i].iov_len;
	g_tx_pkts++;
	g_tx_bytes += (unsigned long)total;
	if (g_trace == 1)
		log_frame("TX", iov, iovlen, g_tx_pkts);

	/* The kernel tap write(2) takes one whole L2 frame; coalesce the iov. */
	if (iovlen == 1) {
		idontcare = write(viu->viu_fd, iov[0].iov_base, iov[0].iov_len);
	} else {
		char tmp[9018];
		size_t off = 0;
		for (i = 0; i < iovlen && off < sizeof(tmp); i++) {
			size_t c = iov[i].iov_len;
			if (off + c > sizeof(tmp)) c = sizeof(tmp) - off;
			memcpy(tmp + off, iov[i].iov_base, c);
			off += c;
		}
		idontcare = write(viu->viu_fd, tmp, off);
	}

	rumpuser_component_schedule(cookie);
}

int
VIFHYPER_DYING(struct virtif_user *viu)
{
	viu->viu_dying = 1;
	return 0;
}

void
VIFHYPER_DESTROY(struct virtif_user *viu)
{
	void *cookie = rumpuser_component_unschedule();
	viu->viu_dying = 1;
	rumpuser_thread_join(viu->viu_rcvcookie);
	close(viu->viu_fd);
	free(viu);
	rumpuser_component_schedule(cookie);
}
#endif
