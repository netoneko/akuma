/*
 * hijack.c - a minimal librumphijack-equivalent, built into a SINGLE LD_PRELOAD
 * .so that statically embeds the whole NetBSD rump TCP/IP stack + our rumpuser.
 *
 * Goal: run an UNMODIFIED dynamic binary (e.g. busybox wget) so that its network
 * syscalls are served by the in-process NetBSD rump stack instead of the host
 * kernel - proving "unmodified binary on the correct stack". No rump server / no
 * sysproxy: the rump kernel lives in the same address space, brought up by this
 * library constructor.
 *
 * Mechanism:
 *  - constructor: rump_init() + create/address/up virt0 (the virtif NIC, backed
 *    by the host TAP via the instrumented virtif backend) + default route.
 *  - libc interposition: socket, connect, send, recv, read, write, close, poll,
 *    fcntl, setsockopt are overridden. AF_INET/AF_INET6 sockets are created in the
 *    rump fd space and handed back with a high offset (RUMP_FDOFF) so we can tell
 *    them apart from real libc fds; calls on those fds route to rump_sys_*, and
 *    everything else falls through to the real libc symbol (dlsym RTLD_NEXT).
 *  - Linux sockaddr_in is translated to NetBSD layout (which has sin_len).
 *
 * Scope: the wget GET path (no DNS - use an IP URL). Not a complete hijack.
 */
#define _GNU_SOURCE
#include <sys/types.h>   /* dev_t / u_long for the rump VFS decls pulled in by rump.h */
#include <dlfcn.h>
#include <errno.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <netinet/in.h>

#include <rump/rump.h>
#include <rump/rump_syscalls.h>
#include <rump/netconfig.h>

void virtif_dump_stats(void);   /* from virtif_user_instr.c */

#define RUMP_FDOFF 0x40000000     /* rump fds carry this offset in the app's view */
#define ISRUMP(fd) ((fd) >= RUMP_FDOFF)
#define R(fd)      ((fd) - RUMP_FDOFF)

/* AF_INET == 2 on both Linux and NetBSD; AF_INET6 differs (Linux 10, NetBSD 24). */
#define LINUX_AF_INET6  10
#define NETBSD_AF_INET6 24

/* NetBSD sockaddr_in: has sin_len, 1-byte sin_family. 16 bytes. */
struct nb_sockaddr_in {
	uint8_t  sin_len;
	uint8_t  sin_family;
	uint16_t sin_port;
	uint32_t sin_addr;
	uint8_t  sin_zero[8];
};

/* Real libc entry points. */
static int  (*real_connect)(int, const struct sockaddr *, socklen_t);
static ssize_t (*real_read)(int, void *, size_t);
static ssize_t (*real_write)(int, const void *, size_t);
static int  (*real_close)(int);
static int  (*real_poll)(struct pollfd *, nfds_t, int);
static int  (*real_fcntl)(int, int, ...);
static ssize_t (*real_readv)(int, const struct iovec *, int);
static ssize_t (*real_writev)(int, const struct iovec *, int);

static void
resolve(void)
{
	real_connect = dlsym(RTLD_NEXT, "connect");
	real_read    = dlsym(RTLD_NEXT, "read");
	real_write   = dlsym(RTLD_NEXT, "write");
	real_close   = dlsym(RTLD_NEXT, "close");
	real_poll    = dlsym(RTLD_NEXT, "poll");
	real_fcntl   = dlsym(RTLD_NEXT, "fcntl");
	real_readv   = dlsym(RTLD_NEXT, "readv");
	real_writev  = dlsym(RTLD_NEXT, "writev");
}

/* Bring up the rump stack once, before the app's main(). */
__attribute__((constructor))
static void
hijack_init(void)
{
	const char *ifaddr = getenv("RUMP_IFADDR");   /* default 10.0.0.2 */
	const char *ifmask = getenv("RUMP_IFMASK");   /* default 255.255.255.0 */
	const char *gw     = getenv("RUMP_GW");        /* default 10.0.0.1 */
	int rv;

	const char *dhcp = getenv("RUMP_DHCP");        /* set => DHCP instead of static */

	resolve();

	if (!ifaddr) ifaddr = "10.0.0.2";
	if (!ifmask) ifmask = "255.255.255.0";
	if (!gw)     gw     = "10.0.0.1";

	fprintf(stderr, "[hijack] rump_init...\n");
	if ((rv = rump_init()) != 0) {
		fprintf(stderr, "[hijack] rump_init failed: %d\n", rv);
		return;
	}
	rv = rump_pub_netconfig_ifcreate("virt0");
	fprintf(stderr, "[hijack] ifcreate virt0 -> %d\n", rv);

	if (dhcp && *dhcp && *dhcp != '0') {
		/* M1 path: lease an address (and default route) from a DHCP server on
		 * the wire, exactly like a real box would. ifup happens inside. */
		rv = rump_pub_netconfig_dhcp_ipv4_oneshot("virt0");
		fprintf(stderr, "[hijack] dhcp_ipv4_oneshot -> %d\n", rv);
	} else {
		rv = rump_pub_netconfig_ipv4_ifaddr("virt0", ifaddr, ifmask);
		fprintf(stderr, "[hijack] ipv4_ifaddr %s/%s -> %d\n", ifaddr, ifmask, rv);
		rv = rump_pub_netconfig_ifup("virt0");
		fprintf(stderr, "[hijack] ifup -> %d\n", rv);
		rv = rump_pub_netconfig_ipv4_gw(gw);
		fprintf(stderr, "[hijack] gw %s -> %d\n", gw, rv);
	}

	atexit(virtif_dump_stats);
	fprintf(stderr, "[hijack] stack up; interposing socket calls.\n");
}

/* Translate a Linux sockaddr to NetBSD layout in `out`; returns NetBSD len. */
static socklen_t
xlate_sockaddr(const struct sockaddr *sa, struct nb_sockaddr_in *out)
{
	const struct sockaddr_in *li = (const struct sockaddr_in *)sa;
	memset(out, 0, sizeof(*out));
	out->sin_len = sizeof(*out);
	out->sin_family = 2;                  /* AF_INET */
	out->sin_port = li->sin_port;         /* already network order */
	out->sin_addr = li->sin_addr.s_addr;  /* already network order */
	return sizeof(*out);
}

/* ── interposed calls ────────────────────────────────────────────────────── */

int
socket(int domain, int type, int protocol)
{
	if (domain == AF_INET || domain == LINUX_AF_INET6) {
		int nbdom = (domain == LINUX_AF_INET6) ? NETBSD_AF_INET6 : 2;
		/* Strip Linux-only type bits (SOCK_NONBLOCK=0x800, SOCK_CLOEXEC=0x80000)
		 * that NetBSD doesn't accept in the type arg. We keep the rump socket
		 * blocking (fcntl() above ignores O_NONBLOCK) so curl's connect/send/recv
		 * are synchronous and dodge the nonblocking-connect + getsockopt dance. */
		int nbtype = type & ~(0x800 | 0x80000);
		int rfd = rump_sys_socket(nbdom, nbtype, protocol);
		if (rfd < 0)
			return -1;
		return rfd + RUMP_FDOFF;
	}
	/* non-IP sockets stay on the host */
	int (*real)(int, int, int) = dlsym(RTLD_NEXT, "socket");
	return real(domain, type, protocol);
}

int
connect(int fd, const struct sockaddr *addr, socklen_t len)
{
	if (ISRUMP(fd)) {
		struct nb_sockaddr_in nb;
		socklen_t nblen = xlate_sockaddr(addr, &nb);
		return rump_sys_connect(R(fd), (struct sockaddr *)&nb, nblen);
	}
	return real_connect(fd, addr, len);
}

ssize_t
read(int fd, void *buf, size_t n)
{
	if (ISRUMP(fd))
		return rump_sys_read(R(fd), buf, n);
	return real_read(fd, buf, n);
}

ssize_t
write(int fd, const void *buf, size_t n)
{
	if (ISRUMP(fd))
		return rump_sys_write(R(fd), buf, n);
	return real_write(fd, buf, n);
}

/* musl stdio (fdopen/fgets/fputs/fflush) does its socket I/O via readv/writev,
 * NOT read/write — so these are the ones that actually carry an app's HTTP. */
ssize_t
readv(int fd, const struct iovec *iov, int iovcnt)
{
	if (ISRUMP(fd))
		return rump_sys_readv(R(fd), iov, iovcnt);
	return real_readv(fd, iov, iovcnt);
}

ssize_t
writev(int fd, const struct iovec *iov, int iovcnt)
{
	if (ISRUMP(fd))
		return rump_sys_writev(R(fd), iov, iovcnt);
	return real_writev(fd, iov, iovcnt);
}

ssize_t
send(int fd, const void *buf, size_t n, int flags)
{
	if (ISRUMP(fd))
		return rump_sys_sendto(R(fd), buf, n, flags, NULL, 0);
	ssize_t (*real)(int, const void *, size_t, int) = dlsym(RTLD_NEXT, "send");
	return real(fd, buf, n, flags);
}

ssize_t
recv(int fd, void *buf, size_t n, int flags)
{
	if (ISRUMP(fd))
		return rump_sys_recvfrom(R(fd), buf, n, flags, NULL, 0);
	ssize_t (*real)(int, void *, size_t, int) = dlsym(RTLD_NEXT, "recv");
	return real(fd, buf, n, flags);
}

int
close(int fd)
{
	if (ISRUMP(fd))
		return rump_sys_close(R(fd));
	return real_close(fd);
}

int
poll(struct pollfd *fds, nfds_t nfds, int timeout)
{
	/* If any fd is a rump fd, run the whole set through rump (this demo's
	 * sockets are all rump fds). Translate the offset in/out. */
	nfds_t i;
	int anyrump = 0;
	for (i = 0; i < nfds; i++)
		if (ISRUMP(fds[i].fd)) { anyrump = 1; break; }
	if (!anyrump)
		return real_poll(fds, nfds, timeout);
	for (i = 0; i < nfds; i++)
		if (ISRUMP(fds[i].fd)) fds[i].fd = R(fds[i].fd);
	int rv = rump_sys_poll(fds, nfds, timeout);
	for (i = 0; i < nfds; i++)
		fds[i].fd += RUMP_FDOFF;
	return rv;
}

/* F_* command numbers happen to match (Linux/NetBSD: GETFD=1 SETFD=2 GETFL=3
 * SETFL=4), but O_NONBLOCK differs (Linux 0x800 vs NetBSD 0x4). Rather than
 * translate flag-by-flag, keep rump sockets BLOCKING and answer fcntl locally:
 * curl then does a blocking connect()/send()/recv() (connect returns 0 on
 * success, no nonblocking+getsockopt dance), which sidesteps every constant
 * mismatch. */
#define LINUX_F_GETFL 3
int
fcntl(int fd, int cmd, ...)
{
	va_list ap;
	long arg;
	va_start(ap, cmd);
	arg = va_arg(ap, long);
	va_end(ap);
	if (ISRUMP(fd)) {
		if (cmd == LINUX_F_GETFL)
			return 0x0002;   /* O_RDWR, no O_NONBLOCK */
		return 0;            /* F_SETFL / F_SETFD / ... : accept, keep blocking */
	}
	return real_fcntl(fd, cmd, arg);
}

ssize_t
sendto(int fd, const void *buf, size_t n, int flags,
	const struct sockaddr *to, socklen_t tolen)
{
	if (ISRUMP(fd)) {
		if (to) {
			struct nb_sockaddr_in nb;
			socklen_t nblen = xlate_sockaddr(to, &nb);
			return rump_sys_sendto(R(fd), buf, n, flags,
			    (struct sockaddr *)&nb, nblen);
		}
		return rump_sys_sendto(R(fd), buf, n, flags, NULL, 0);
	}
	ssize_t (*real)(int, const void *, size_t, int,
	    const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "sendto");
	return real(fd, buf, n, flags, to, tolen);
}

ssize_t
recvfrom(int fd, void *buf, size_t n, int flags,
	struct sockaddr *from, socklen_t *fromlen)
{
	if (ISRUMP(fd))
		return rump_sys_recvfrom(R(fd), buf, n, flags, NULL, NULL);
	ssize_t (*real)(int, void *, size_t, int, struct sockaddr *, socklen_t *) =
	    dlsym(RTLD_NEXT, "recvfrom");
	return real(fd, buf, n, flags, from, fromlen);
}

/* curl checks getsockopt(SO_ERROR) after connect; answer "no error" so it
 * proceeds. (Linux SOL_SOCKET=1, SO_ERROR=4.) */
int
getsockopt(int fd, int level, int optname, void *optval, socklen_t *optlen)
{
	if (ISRUMP(fd)) {
		if (level == 1 && optname == 4 && optval && optlen && *optlen >= 4) {
			*(int *)optval = 0;
			*optlen = 4;
		} else if (optval && optlen && *optlen >= 4) {
			*(int *)optval = 0;
		}
		return 0;
	}
	int (*real)(int, int, int, void *, socklen_t *) =
	    dlsym(RTLD_NEXT, "getsockopt");
	return real(fd, level, optname, optval, optlen);
}

/* setsockopt/getsockopt: best-effort — route to rump, but never fail the app on
 * an option rump doesn't grok (wget sets a few cosmetic ones). */
int
setsockopt(int fd, int level, int optname, const void *val, socklen_t len)
{
	if (ISRUMP(fd)) {
		(void)rump_sys_setsockopt(R(fd), level, optname, val, len);
		return 0;
	}
	int (*real)(int, int, int, const void *, socklen_t) =
	    dlsym(RTLD_NEXT, "setsockopt");
	return real(fd, level, optname, val, len);
}
