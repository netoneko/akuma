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
#include <unistd.h>

#include <rump/rump.h>
#include <rump/netconfig.h>

void virtif_dump_stats(void);   /* from rumpcomp_tap.c (best-effort) */

int
main(int argc, char **argv)
{
	const char *url    = (argc > 1) ? argv[1] : "unix:///tmp/rump_server.sock";
	const char *ifname = (argc > 2) ? argv[2] : "virt0";
	int rv;

	setvbuf(stdout, NULL, _IONBF, 0);

	printf("RUMP_SERVER: rump_init...\n");
	if ((rv = rump_init()) != 0) {
		printf("RUMP_SERVER: FAIL rump_init=%d\n", rv);
		return 1;
	}

	rv = rump_pub_netconfig_ifcreate(ifname);
	printf("RUMP_SERVER: ifcreate %s -> %d\n", ifname, rv);

	rv = rump_pub_netconfig_dhcp_ipv4_oneshot(ifname);
	printf("RUMP_SERVER: dhcp_ipv4_oneshot %s -> %d\n", ifname, rv);
	if (rv != 0) {
		/* Not fatal for proving the sysproxy listener: a client can still
		 * share the stack (e.g. loopback) even if DHCP didn't complete. */
		printf("RUMP_SERVER: WARN — DHCP rv=%d (continuing; stack still served)\n", rv);
	}

	rv = rump_init_server(url);
	printf("RUMP_SERVER: rump_init_server(%s) -> %d\n", url, rv);
	if (rv != 0) {
		printf("RUMP_SERVER: FAIL — rump_init_server rv=%d\n", rv);
		return 1;
	}

	printf("RUMP_SERVER: LISTENING — sysproxy on %s (stack iface %s). "
	    "Clients: RUMP_SERVER=%s rumpclient ...\n", url, ifname, url);

	/* The sp server runs its own accept/worker threads; just stay alive. */
	for (;;)
		pause();

	return 0;
}
