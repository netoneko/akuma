/*
 * virtif_user_instr.c — the stock NetBSD virtif TUN/TAP backend
 * (sys/rump/net/lib/libvirtif/virtif_user.c) with INSTRUMENTATION added at the
 * rump↔wire seam, to PROVE that traffic from an app rides the NetBSD stack.
 *
 * Every frame the NetBSD stack hands to the NIC passes through VIFHYPER_SEND (TX)
 * and every frame off the wire passes through the rcvthread → VIF_DELIVERPKT (RX).
 * smoltcp has no virtif, so a packet counted here DID go through the rump stack.
 *
 * Added (vs. stock): global TX/RX packet+byte counters, an optional per-packet
 * Ethernet-header log (env RUMP_VIRTIF_TRACE=1), and an exported
 * virtif_dump_stats() the demo/shim calls at exit. Kept otherwise verbatim so it
 * stays a faithful stand-in for the stock backend.
 */
#ifndef _KERNEL
#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/uio.h>

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifdef __linux__
#include <net/if.h>
#include <linux/if_tun.h>
#endif

#include <rump/rumpuser_component.h>

#include "if_virt.h"
#include "virtif_user.h"

#if VIFHYPER_REVISION != 20140313
#error VIFHYPER_REVISION mismatch
#endif

/* ── instrumentation ─────────────────────────────────────────────────────── */
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
	size_t total = 0;
	size_t i;
	unsigned char *p;

	for (i = 0; i < iovlen; i++)
		total += iov[i].iov_len;
	fprintf(stderr, "[VIRTIF %s #%lu] %zu bytes", dir, seq, total);
	if (iovlen > 0 && iov[0].iov_len >= 14) {
		p = iov[0].iov_base;
		fprintf(stderr,
		    " dst=%02x:%02x:%02x:%02x:%02x:%02x"
		    " src=%02x:%02x:%02x:%02x:%02x:%02x ethtype=0x%02x%02x",
		    p[0], p[1], p[2], p[3], p[4], p[5],
		    p[6], p[7], p[8], p[9], p[10], p[11], p[12], p[13]);
	}
	fprintf(stderr, "\n");
}

/* Exported: dump totals (call at exit). PROOF line for the demo. */
void
virtif_dump_stats(void)
{
	fprintf(stderr,
	    "[VIRTIF STATS] tx=%lu pkts/%lu bytes  rx=%lu pkts/%lu bytes "
	    "(all carried by the NetBSD rump stack, not smoltcp)\n",
	    g_tx_pkts, g_tx_bytes, g_rx_pkts, g_rx_bytes);
}

/* ── stock backend (verbatim, with the two count/log hooks) ──────────────── */
struct virtif_user {
	struct virtif_sc *viu_virtifsc;
	int viu_devnum;

	int viu_fd;
	int viu_pipe[2];
	pthread_t viu_rcvthr;

	int viu_dying;

	char viu_rcvbuf[9018]; /* jumbo frame max len */
};

static int
opentapdev(int devnum)
{
	int fd = -1;
#if defined(__linux__)
	struct ifreq ifr;
	char devname[16];

	fd = open("/dev/net/tun", O_RDWR);
	if (fd == -1) {
		fprintf(stderr, "rumpcomp_virtif_create: can't open %s: %s\n",
		    "/dev/net/tun", strerror(errno));
		return -1;
	}

	snprintf(devname, sizeof(devname), "tun%d", devnum);
	memset(&ifr, 0, sizeof(ifr));
	ifr.ifr_flags = IFF_TAP | IFF_NO_PI;
	strncpy(ifr.ifr_name, devname, sizeof(ifr.ifr_name) - 1);

	if (ioctl(fd, TUNSETIFF, &ifr) == -1) {
		fprintf(stderr, "rumpcomp_virtif_create: TUNSETIFF %s: %s\n",
		    devname, strerror(errno));
		close(fd);
		fd = -1;
	}
#else
	fprintf(stderr, "virtif not supported on this platform\n");
#endif
	return fd;
}

static void
closetapdev(struct virtif_user *viu)
{
	close(viu->viu_fd);
}

static void *
rcvthread(void *aaargh)
{
	struct virtif_user *viu = aaargh;
	struct pollfd pfd[2];
	struct iovec iov;
	ssize_t nn = 0;
	int prv;

	rumpuser_component_kthread();

	pfd[0].fd = viu->viu_fd;
	pfd[0].events = POLLIN;
	pfd[1].fd = viu->viu_pipe[0];
	pfd[1].events = POLLIN;

	while (!viu->viu_dying) {
		prv = poll(pfd, 2, -1);
		if (prv == 0)
			continue;
		if (prv == -1) {
			fprintf(stderr, "virt%d: poll error: %d\n",
			    viu->viu_devnum, errno);
			sleep(1);
			continue;
		}
		if (pfd[1].revents & POLLIN)
			continue;

		nn = read(viu->viu_fd, viu->viu_rcvbuf, sizeof(viu->viu_rcvbuf));
		if (nn == -1 && errno == EAGAIN)
			continue;
		if (nn < 1) {
			fprintf(stderr, "virt%d: receive failed\n",
			    viu->viu_devnum);
			sleep(1);
			continue;
		}
		iov.iov_base = viu->viu_rcvbuf;
		iov.iov_len = nn;

		/* INSTRUMENT: RX — a frame off the wire into the NetBSD stack. */
		g_rx_pkts++;
		g_rx_bytes += (unsigned long)nn;
		if (g_trace == 1)
			log_frame("RX", &iov, 1, g_rx_pkts);

		rumpuser_component_schedule(NULL);
		VIF_DELIVERPKT(viu->viu_virtifsc, &iov, 1);
		rumpuser_component_unschedule();
	}

	assert(viu->viu_dying);
	rumpuser_component_kthread_release();
	return NULL;
}

int
VIFHYPER_CREATE(const char *devstr, struct virtif_sc *vif_sc, uint8_t *enaddr,
	struct virtif_user **viup)
{
	struct virtif_user *viu = NULL;
	void *cookie;
	int devnum;
	int rv;

	trace_init();
	cookie = rumpuser_component_unschedule();

	devnum = atoi(devstr);

	viu = calloc(1, sizeof(*viu));
	if (viu == NULL) {
		rv = errno;
		goto oerr1;
	}
	viu->viu_virtifsc = vif_sc;

	viu->viu_fd = opentapdev(devnum);
	if (viu->viu_fd == -1) {
		rv = errno;
		goto oerr2;
	}
	viu->viu_devnum = devnum;

	if (pipe(viu->viu_pipe) == -1) {
		rv = errno;
		goto oerr3;
	}

	if ((rv = pthread_create(&viu->viu_rcvthr, NULL, rcvthread, viu)) != 0)
		goto oerr4;

	rumpuser_component_schedule(cookie);
	*viup = viu;
	return 0;

 oerr4:
	close(viu->viu_pipe[0]);
	close(viu->viu_pipe[1]);
 oerr3:
	closetapdev(viu);
 oerr2:
	free(viu);
 oerr1:
	rumpuser_component_schedule(cookie);
	return rumpuser_component_errtrans(rv);
}

void
VIFHYPER_SEND(struct virtif_user *viu, struct iovec *iov, size_t iovlen)
{
	void *cookie = rumpuser_component_unschedule();
	ssize_t idontcare __attribute__((__unused__));
	size_t i, total = 0;

	/* INSTRUMENT: TX — a frame the NetBSD stack is putting on the wire. */
	for (i = 0; i < iovlen; i++)
		total += iov[i].iov_len;
	g_tx_pkts++;
	g_tx_bytes += (unsigned long)total;
	if (g_trace == 1)
		log_frame("TX", iov, iovlen, g_tx_pkts);

	idontcare = writev(viu->viu_fd, iov, iovlen);

	rumpuser_component_schedule(cookie);
}

int
VIFHYPER_DYING(struct virtif_user *viu)
{
	void *cookie = rumpuser_component_unschedule();

	viu->viu_dying = 1;
	if (write(viu->viu_pipe[1], &viu->viu_dying, sizeof(viu->viu_dying)) == -1)
		fprintf(stderr, "%s: failed to signal thread\n",
		    VIF_STRING(VIFHYPER_DYING));

	rumpuser_component_schedule(cookie);
	return 0;
}

void
VIFHYPER_DESTROY(struct virtif_user *viu)
{
	void *cookie = rumpuser_component_unschedule();

	pthread_join(viu->viu_rcvthr, NULL);
	closetapdev(viu);
	close(viu->viu_pipe[0]);
	close(viu->viu_pipe[1]);
	free(viu);

	rumpuser_component_schedule(cookie);
}
#endif
