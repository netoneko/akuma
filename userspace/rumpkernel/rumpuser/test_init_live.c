/*
 * test_init_live.c — like test_init.c but stays alive after rump_init() so the
 * process can be inspected with `ps`, to compare OS-thread (kthread) counts
 * between the pthread and fiber rumpuser backends:
 *   pthread build: rump_init spawns ~19 host pthreads  -> ~19 child PIDs
 *   fiber   build: rump kthreads are cooperative fibers -> 0 child PIDs
 */
#include <sys/types.h>
#include <inttypes.h>
#include <stdio.h>
#include <unistd.h>
#include <rump/rump.h>

int
main(void)
{
	int rv = rump_init();
	printf("rump_init rv=%d (%s) — staying alive for ps\n", rv, rv == 0 ? "PASS" : "FAIL");
	fflush(stdout);
	for (;;)
		pause();
	return rv;
}
