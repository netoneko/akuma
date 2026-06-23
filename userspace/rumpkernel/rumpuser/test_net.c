/*
 * test_net.c — Phase 4 exit test: after rump_init(), bring up a virtif NIC
 * (virt0, backed by the host TUN/TAP via the stock virtif_user.c backend),
 * assign a static IPv4 address through the NetBSD stack, mark it up, and open a
 * socket — proving the rump TCP/IP stack configures + runs networking on our
 * Rust rumpuser. Built/run in the Linux container (docker-net-test.sh) with
 * /dev/net/tun. Based on buildrump.sh/tests/nettest_simple/.
 *
 * This is the in-container proof (path #2). The Akuma version swaps the stock
 * TUN/TAP backend for our rumpcomp_user over /dev/net/tap0.
 */
#include <sys/types.h>

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>

#include <rump/rump.h>
#include <rump/rump_syscalls.h>
#include <rump/netconfig.h>

/* NetBSD numeric constants (independent of the host's <sys/socket.h>). */
#define NB_AF_INET	2
#define NB_SOCK_DGRAM	2

#define STEP(a) do {							\
	int rv = (a);							\
	printf("RUMPNET-AKUMA: %-40s -> %d\n", #a, rv);			\
	if (rv != 0) {							\
		printf("RUMPNET-AKUMA: FAIL at %s (rv=%d)\n", #a, rv);	\
		return 1;						\
	}								\
} while (0)

int
main(void)
{
	/* Unbuffered: if a later step hangs (e.g. scheduler deadlock in the RX
	 * kthread), the progress printed so far still reaches the pipe. */
	setvbuf(stdout, NULL, _IONBF, 0);

	int rv = rump_init();
	printf("RUMPNET-AKUMA: rump_init() returned %d\n", rv);
	if (rv != 0) {
		printf("RUMPNET-AKUMA: FAIL — rump_init rv=%d\n", rv);
		return rv;
	}

	/*
	 * virt0: VIRTIF_BASE=virt ⇒ ifname "virt". This if_virt.c is built WITHOUT
	 * RUMP_VIF_LINKSTR, so virtif_clone creates the interface immediately at
	 * ifcreate, binding it to /dev/net/tun unit N from the unit number (virt0 ⇒
	 * tun0). SIOCSLINKSTR isn't compiled in (ifsetlinkstr would return ENOTTY),
	 * so we go straight to addressing.
	 */
	STEP(rump_pub_netconfig_ifcreate("virt0"));
	STEP(rump_pub_netconfig_ipv4_ifaddr("virt0", "10.0.0.2", "255.255.255.0"));
	STEP(rump_pub_netconfig_ifup("virt0"));

	/* The socket path through the NetBSD stack. */
	int s = rump_sys_socket(NB_AF_INET, NB_SOCK_DGRAM, 0);
	printf("RUMPNET-AKUMA: rump_sys_socket(AF_INET,DGRAM) -> %d\n", s);
	if (s < 0) {
		printf("RUMPNET-AKUMA: FAIL — socket fd=%d\n", s);
		return 1;
	}
	rump_sys_close(s);

	printf("RUMPNET-AKUMA: PASS — virt0 up with 10.0.0.2/24 + socket OK "
	    "on our rumpuser\n");
	return 0;
}
