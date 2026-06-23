/*
 * rumpserver.c — step 1 toward acceptance/11: an INBOUND TCP server on the NetBSD
 * rump stack, inside an Akuma box, reachable from the host. Proves bind/listen/
 * accept + recv/send over rump (the foundation an sshd needs), end-to-end through
 * /dev/net/tap0 and QEMU's net1 SLIRP hostfwd.
 *
 * Static Akuma binary (rump + rumpuser + rumpcomp_tap.c). Run in a RUMP_NIC=1 box
 * with QEMU `hostfwd=tcp::2223-:<port>` on net1 (the RUMP_SSH_PORT plumbing). Then
 * from the host: connect to localhost:2223, get a banner, lines are echoed.
 *
 *   rumpserver [port]      default: 22   (matches the RUMP_SSH_PORT hostfwd)
 */
#include <sys/types.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <rump/rump.h>
#include <rump/rump_syscalls.h>
#include <rump/netconfig.h>

void virtif_dump_stats(void);

#define NB_AF_INET     2
#define NB_SOCK_STREAM 1

struct nb_sockaddr_in {
	uint8_t  sin_len;
	uint8_t  sin_family;
	uint16_t sin_port;
	uint32_t sin_addr;
	uint8_t  sin_zero[8];
};

int
main(int argc, char **argv)
{
	int port = (argc > 1) ? atoi(argv[1]) : 22;
	int rv, ls, cs;

	setvbuf(stdout, NULL, _IONBF, 0);

	printf("RUMPSERVER: rump_init...\n");
	if ((rv = rump_init()) != 0) { printf("RUMPSERVER: FAIL rump_init=%d\n", rv); return 1; }
	rv = rump_pub_netconfig_ifcreate("virt0");
	printf("RUMPSERVER: ifcreate virt0 -> %d\n", rv);
	rv = rump_pub_netconfig_dhcp_ipv4_oneshot("virt0");
	printf("RUMPSERVER: dhcp_ipv4_oneshot -> %d\n", rv);
	if (rv != 0) { virtif_dump_stats(); return 1; }

	ls = rump_sys_socket(NB_AF_INET, NB_SOCK_STREAM, 0);
	printf("RUMPSERVER: socket -> %d\n", ls);
	if (ls < 0) { virtif_dump_stats(); return 1; }

	struct nb_sockaddr_in sa;
	memset(&sa, 0, sizeof(sa));
	sa.sin_len = sizeof(sa);
	sa.sin_family = NB_AF_INET;
	sa.sin_port = (uint16_t)((port >> 8) | (port << 8));   /* htons */
	sa.sin_addr = 0;                                        /* INADDR_ANY */

	rv = rump_sys_bind(ls, (struct sockaddr *)&sa, sizeof(sa));
	printf("RUMPSERVER: bind 0.0.0.0:%d -> %d\n", port, rv);
	if (rv != 0) { virtif_dump_stats(); return 1; }

	rv = rump_sys_listen(ls, 5);
	printf("RUMPSERVER: listen -> %d\n", rv);
	if (rv != 0) { virtif_dump_stats(); return 1; }

	printf("RUMPSERVER: LISTENING on the NetBSD rump stack — connect via host :2223\n");

	/* Accept a few connections; banner + echo each line, then close. */
	for (int conn = 0; conn < 8; conn++) {
		struct nb_sockaddr_in peer;
		uint32_t plen = sizeof(peer);
		cs = rump_sys_accept(ls, (struct sockaddr *)&peer, &plen);
		if (cs < 0) { printf("RUMPSERVER: accept -> %d\n", cs); break; }
		uint32_t a = peer.sin_addr;
		printf("RUMPSERVER: accepted conn #%d from %u.%u.%u.%u (fd=%d)\n",
		    conn, a & 0xff, (a >> 8) & 0xff, (a >> 16) & 0xff, (a >> 24) & 0xff, cs);

		const char *banner = "HELLO-FROM-NETBSD-RUMP-STACK-IN-AN-AKUMA-BOX\n";
		rump_sys_write(cs, banner, (int)strlen(banner));

		char buf[512];
		ssize_t n;
		while ((n = rump_sys_read(cs, buf, sizeof(buf))) > 0) {
			rump_sys_write(cs, buf, n);            /* echo */
			if (memchr(buf, '\n', n)) break;       /* one line, then done */
		}
		rump_sys_close(cs);
		printf("RUMPSERVER: conn #%d closed\n", conn);
	}

	rump_sys_close(ls);
	virtif_dump_stats();
	printf("RUMPSERVER: done\n");
	return 0;
}
