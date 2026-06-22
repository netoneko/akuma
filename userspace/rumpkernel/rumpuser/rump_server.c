/*
 * rump_server.c — RUMP_SYSPROXY.md Step 2: the per-box rump SERVER payload.
 *
 * A long-lived process that owns one NetBSD rump TCP/IP stack and exposes it over
 * a sysproxy unix socket, so other processes (eventually: the Akuma kernel acting
 * as a client on behalf of in-box binaries — Step 4) can run rump_sys_* against
 * this stack. This is the "stack daemon" for a `--net` box.
 *
 *   rump_server [unix-url] [ifname]
 *       unix-url  default: unix:///tmp/rump_server.sock
 *       ifname    default: virt0   (brought up + DHCP'd via /dev/net/tap0 on Akuma)
 *
 * Boot sequence:
 *   rump_init()                         -> NetBSD rump kernel on our rumpuser
 *   rump_pub_netconfig_ifcreate(if)     -> create virt0 (libvirtif over our backend)
 *   rump_pub_netconfig_dhcp_ipv4_oneshot-> DHCP an address (QEMU SLIRP on NIC1)
 *   rump_init_server(url)               -> start the sysproxy listener (rumpuser_sp_init)
 *   pause forever                       -> the sp server runs its own worker threads
 *
 * This file is original Akuma code; it only *calls* the rump public API + our
 * rumpuser backend. The sysproxy server itself is NetBSD source
 * (src-netbsd/lib/librumpuser/rumpuser_sp.c + sp_common.c), compiled separately.
 */
#include <sys/types.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>

#include <rump/rump.h>
#include <rump/netconfig.h>

void virtif_dump_stats(void);   /* from rumpcomp_tap.c (best-effort) */

/* serve on a pre-connected fd (kernel-pipe transport); from sp_serve_fd.c */
extern int rumpuser_sp_init_fd(int, const char *, const char *, const char *);

/*
 * Redirect stdout+stderr to `path` (creating parent dirs best-effort) so all of
 * rump_server's output — including rump_init's verbose dprintf on fd 2 — lands
 * in the box log instead of the (undrained) kernel ProcessChannel. Used by the
 * kernel demo: --log /var/log/box/<id>/rump_server.log.
 */
static void
redirect_log(const char *path)
{
	char tmp[256];
	size_t n = strlen(path);
	if (n >= sizeof(tmp))
		return;
	memcpy(tmp, path, n + 1);
	/* mkdir each parent component (ignore EEXIST/errors). */
	for (char *p = tmp + 1; *p; p++) {
		if (*p == '/') {
			*p = '\0';
			mkdir(tmp, 0755);
			*p = '/';
		}
	}
	int lf = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	if (lf >= 0) {
		dup2(lf, 1); /* stdout (printf) */
		dup2(lf, 2); /* stderr (rumpuser dprintf) */
		if (lf > 2)
			close(lf);
	}
}

int
main(int argc, char **argv)
{
	const char *url    = "unix:///tmp/rump_server.sock";
	const char *ifname = "virt0";
	const char *logpath = NULL; /* --log: redirect stdout/stderr to this file */
	int serve_fd = -1;   /* >=0: serve sysproxy on this inherited fd */
	int do_net = 0;      /* --net: bring up virt0 + DHCP over /dev/net/tap0 */
	int rv;

	/*
	 * Modes:
	 *   rump_server --fd N [--net]     serve on inherited fd N (Akuma kernel-pipe)
	 *   rump_server [url] [--net]      listen on a URL (container/path tests)
	 * --net brings the rump stack online (needs RUMP_NIC=1 / a tap); without it
	 * the stack still serves control-plane syscalls (e.g. socket()).
	 */
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--fd") && i + 1 < argc) {
			serve_fd = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--net")) {
			do_net = 1;
		} else if (!strcmp(argv[i], "--if") && i + 1 < argc) {
			ifname = argv[++i];
		} else if (!strcmp(argv[i], "--log") && i + 1 < argc) {
			logpath = argv[++i];
		} else if (argv[i][0] != '-') {
			url = argv[i];
		}
	}

	if (logpath)
		redirect_log(logpath);

	setvbuf(stdout, NULL, _IONBF, 0);

	printf("RUMP_SERVER: rump_init...\n");
	if ((rv = rump_init()) != 0) {
		printf("RUMP_SERVER: FAIL rump_init=%d\n", rv);
		return 1;
	}

	if (do_net) {
		rv = rump_pub_netconfig_ifcreate(ifname);
		printf("RUMP_SERVER: ifcreate %s -> %d\n", ifname, rv);
		rv = rump_pub_netconfig_dhcp_ipv4_oneshot(ifname);
		printf("RUMP_SERVER: dhcp_ipv4_oneshot %s -> %d\n", ifname, rv);
		if (rv != 0)
			printf("RUMP_SERVER: WARN — DHCP rv=%d (continuing)\n", rv);
	}

	if (serve_fd >= 0) {
		rv = rumpuser_sp_init_fd(serve_fd, "NetBSD", "7.99.34", "evbarm64");
		printf("RUMP_SERVER: rumpuser_sp_init_fd(%d) -> %d\n", serve_fd, rv);
		if (rv != 0) {
			printf("RUMP_SERVER: FAIL — sp_init_fd rv=%d\n", rv);
			return 1;
		}
		printf("RUMP_SERVER: SERVING sysproxy on fd %d (net=%s)\n",
		    serve_fd, do_net ? "up" : "off");
	} else {
		rv = rump_init_server(url);
		printf("RUMP_SERVER: rump_init_server(%s) -> %d\n", url, rv);
		if (rv != 0) {
			printf("RUMP_SERVER: FAIL — rump_init_server rv=%d\n", rv);
			return 1;
		}
		printf("RUMP_SERVER: LISTENING — sysproxy on %s (iface %s)\n", url, ifname);
	}

	/* The sp server runs its own worker threads; just stay alive. */
	for (;;)
		pause();

	return 0;
}
