/*
 * rumphttp.c — the M1 in-box payload: bring up the NetBSD rump TCP/IP stack over
 * /dev/net/tap0, DHCP an address, and HTTP-GET a host through the rump stack — a
 * self-contained "curl" using rump_sys_* directly (NetBSD ABI, no translation).
 *
 * Built as a STATIC Akuma binary (aarch64-linux-musl-gcc) linked with the rump
 * libs + our rumpuser + the /dev/net/tap0 backend (rumpcomp_tap.c). Run inside an
 * Akuma box booted with RUMP_NIC=1 (so /dev/net/tap0 is backed by NIC1's SLIRP,
 * which also serves DHCP and NATs to the QEMU host at 10.0.2.2).
 *
 *   rumphttp [host] [port]      default: 10.0.2.2 80   (the QEMU host)
 */
#include <sys/types.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <rump/rump.h>
#include <rump/rump_syscalls.h>
#include <rump/netconfig.h>

void virtif_dump_stats(void);   /* from rumpcomp_tap.c */

#define NB_AF_INET    2
#define NB_SOCK_STREAM 1

struct nb_sockaddr_in {
	uint8_t  sin_len;
	uint8_t  sin_family;
	uint16_t sin_port;
	uint32_t sin_addr;
	uint8_t  sin_zero[8];
};

/* tiny dotted-quad → network-order u32 (no libc inet_aton on the rump side). */
static uint32_t
parse_ipv4(const char *s)
{
	unsigned a = 0, b = 0, c = 0, d = 0;
	sscanf(s, "%u.%u.%u.%u", &a, &b, &c, &d);
	return (uint32_t)((a) | (b << 8) | (c << 16) | (d << 24)); /* network order */
}

int
main(int argc, char **argv)
{
	const char *host = (argc > 1) ? argv[1] : "10.0.2.2";
	int port = (argc > 2) ? atoi(argv[2]) : 80;
	int rv, s;

	setvbuf(stdout, NULL, _IONBF, 0);

	printf("RUMPHTTP: rump_init...\n");
	if ((rv = rump_init()) != 0) {
		printf("RUMPHTTP: FAIL rump_init=%d\n", rv);
		return 1;
	}
	rv = rump_pub_netconfig_ifcreate("virt0");
	printf("RUMPHTTP: ifcreate virt0 -> %d\n", rv);

	rv = rump_pub_netconfig_dhcp_ipv4_oneshot("virt0");
	printf("RUMPHTTP: dhcp_ipv4_oneshot -> %d\n", rv);
	if (rv != 0) {
		printf("RUMPHTTP: FAIL — DHCP rv=%d\n", rv);
		virtif_dump_stats();
		return 1;
	}

	s = rump_sys_socket(NB_AF_INET, NB_SOCK_STREAM, 0);
	printf("RUMPHTTP: socket -> %d\n", s);
	if (s < 0) { virtif_dump_stats(); return 1; }

	struct nb_sockaddr_in sa;
	memset(&sa, 0, sizeof(sa));
	sa.sin_len = sizeof(sa);
	sa.sin_family = NB_AF_INET;
	sa.sin_port = (uint16_t)((port >> 8) | (port << 8));   /* htons */
	sa.sin_addr = parse_ipv4(host);

	rv = rump_sys_connect(s, (struct sockaddr *)&sa, sizeof(sa));
	printf("RUMPHTTP: connect %s:%d -> %d\n", host, port, rv);
	if (rv != 0) { virtif_dump_stats(); return 1; }

	char req[256];
	int reqlen = snprintf(req, sizeof(req),
	    "GET / HTTP/1.0\r\nHost: %s\r\nUser-Agent: rumphttp\r\n\r\n", host);
	rv = rump_sys_write(s, req, reqlen);
	printf("RUMPHTTP: sent %d-byte GET -> %d\n", reqlen, rv);

	printf("RUMPHTTP: --- response over the NetBSD rump stack ---\n");
	char buf[2048];
	ssize_t n, total = 0;
	while ((n = rump_sys_read(s, buf, sizeof(buf) - 1)) > 0) {
		buf[n] = '\0';
		fwrite(buf, 1, n, stdout);
		total += n;
	}
	printf("\nRUMPHTTP: --- end (%ld bytes) ---\n", (long)total);
	rump_sys_close(s);

	virtif_dump_stats();
	if (total > 0)
		printf("RUMPHTTP: PASS — fetched %ld bytes over the NetBSD rump stack "
		    "(DHCP + TCP via /dev/net/tap0)\n", (long)total);
	else
		printf("RUMPHTTP: FAIL — no response body\n");
	return total > 0 ? 0 : 1;
}
