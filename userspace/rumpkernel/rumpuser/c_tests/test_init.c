/*
 * test_init.c — Phase 2 exit test: link librump.a + our Rust rumpuser and prove
 * rump_init() returns success. Built/run in the Linux container (see
 * docker-rumpuser-test.sh). Based on buildrump.sh/tests/init/init.c.
 */
#include <sys/types.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>

#include <rump/rump.h>

int
main(void)
{
	int rv = rump_init();
	printf("RUMPUSER-AKUMA: rump_init() returned %d\n", rv);
	if (rv == 0)
		printf("RUMPUSER-AKUMA: PASS — NetBSD rump kernel booted on our rumpuser\n");
	else
		printf("RUMPUSER-AKUMA: FAIL — rump_init rv=%d\n", rv);
	return rv;
}
