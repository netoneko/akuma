/*
 * sp_client_test.c — RUMP_SYSPROXY.md Step 3: prove a SECOND process shares the
 * rump_server stack via the sysproxy (rumpclient) protocol.
 *
 * Links NetBSD's librumpclient (rumpclient.c + rump_syscalls.c built -DRUMP_CLIENT)
 * — NOT librump. rumpclient_init() reads $RUMP_SERVER (a unix:// url) and connects
 * to the rump_server; rump_sys_* then marshal over that socket and execute in the
 * SERVER's NetBSD kernel. A valid fd back proves two processes share one stack.
 *
 *   RUMP_SERVER=unix:///tmp/rs.sock sp_client_test
 *
 * Original Akuma test code; the client library it links is NetBSD source.
 */
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include <rump/rumpclient.h>
#include <rump/rump_syscalls.h>

#define NB_AF_INET     2
#define NB_SOCK_STREAM 1

int
main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);

	if (rumpclient_init() != 0) {
		printf("CLIENT: FAIL rumpclient_init (is RUMP_SERVER set + server up?)\n");
		return 1;
	}
	printf("CLIENT: rumpclient_init OK (connected to %s)\n", getenv("RUMP_SERVER"));

	int s = rump_sys_socket(NB_AF_INET, NB_SOCK_STREAM, 0);
	printf("CLIENT: rump_sys_socket(AF_INET, SOCK_STREAM) -> %d\n", s);
	if (s < 0) {
		printf("CLIENT: FAIL — socket call did not round-trip to the server stack\n");
		return 1;
	}
	rump_sys_close(s);
	printf("CLIENT: PASS — a second process ran rump_sys_* against the shared "
	    "NetBSD stack over the sysproxy socket\n");
	return 0;
}
