/*
 * mmap_scale — does an anonymous `mmap` get more expensive as a process
 * accumulates mappings?
 *
 * Written for docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md §8. §5 fixed
 * `find_free_va`'s O(n^2)-per-call restart and the `zerocopy` wall went away,
 * but `[PSTATS]` then put 8.27 s of `rustc`'s 9.29 s of in-kernel time in
 * **59 043 `mmap` calls** — 140 us each. The residual suspicion is that the
 * per-call cost is still linear in the region count (a full `sort_unstable_by_key`
 * plus a first-fit scan from `MMAP_BASE`, both O(n), on every call), which a
 * build only exposes after `rustc` has accumulated four figures of mappings.
 *
 * A build cannot answer that: it takes a minute and confounds the placer with
 * codegen, faults and ext2. This does one thing — mmap a single page at a time
 * and time each call — and prints the cost bucketed by how many mappings the
 * process already had. Flat means placement is not the cost. Rising means it is,
 * and the slope says by how much.
 *
 * ONE static musl binary, built for both architectures, so it runs unchanged on
 * Akuma/amd64, Akuma/aarch64 and on real Linux as the reference arm — the rule
 * §7 of that doc paid for ("measure with one binary or do not measure").
 *
 * Every number reported is a MINIMUM: contention only ever adds, so the minimum
 * is the closest thing to the cost of the work itself.
 *
 * The minimum is taken over GROUPS of `GROUP` calls, not over whole buckets, and
 * that is not a detail. Timing a 250-call bucket as one bracket and taking the
 * best of `repeat` passes assumes a pass exists in which the whole bucket ran
 * undisturbed — false under QEMU TCG, where the first version of this probe
 * reported buckets alternating between 1.5 us and 25 us with no relation to the
 * region count at all, and a `growth=16.9x` that was purely which bucket got
 * unlucky last. Ten calls per bracket is short enough that most brackets escape
 * preemption and long enough to amortise the two clock reads.
 *
 * usage: mmap_scale [regions] [repeat]
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/mman.h>
#include <unistd.h>

#define PAGE 4096
#define MAX_REGIONS 8192
#define BUCKET 250
/* Calls per timing bracket — see the note on minima in the header. */
#define GROUP 10

static long long now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static void *slots[MAX_REGIONS];
/* Per-bucket minimum across repeats, in ns per call. */
static long long mmap_min[MAX_REGIONS / BUCKET + 1];
static long long munmap_min[MAX_REGIONS / BUCKET + 1];

int main(int argc, char **argv)
{
	int regions = argc > 1 ? atoi(argv[1]) : 4000;
	int repeat = argc > 2 ? atoi(argv[2]) : 3;
	int nb, b, r, i, failed = 0;

	if (regions < BUCKET) regions = BUCKET;
	if (regions > MAX_REGIONS) regions = MAX_REGIONS;
	if (repeat < 1) repeat = 1;
	nb = regions / BUCKET;

	for (b = 0; b < nb; b++) { mmap_min[b] = -1; munmap_min[b] = -1; }

	printf("[mmap_scale] regions=%d repeat=%d bucket=%d page=%d\n",
	       regions, repeat, BUCKET, PAGE);

	for (r = 0; r < repeat; r++) {
		/* Grow: one single-page anonymous mapping at a time. The shape
		 * musl's mallocng produces, and the shape `rustc` accumulates. */
		for (b = 0; b < nb; b++) {
			for (i = 0; i < BUCKET; i += GROUP) {
				long long t0 = now_ns(), t1, per;
				int j;
				for (j = 0; j < GROUP; j++) {
					int idx = b * BUCKET + i + j;
					slots[idx] = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
							  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
					if (slots[idx] == MAP_FAILED) { slots[idx] = NULL; failed++; }
				}
				t1 = now_ns();
				per = (t1 - t0) / GROUP;
				if (mmap_min[b] < 0 || per < mmap_min[b]) mmap_min[b] = per;
			}
		}
		/* Shrink, highest bucket first, so each `munmap` is measured
		 * against a region list still holding everything below it. */
		for (b = nb - 1; b >= 0; b--) {
			for (i = 0; i < BUCKET; i += GROUP) {
				long long t0 = now_ns(), t1, per;
				int j;
				for (j = 0; j < GROUP; j++) {
					int idx = b * BUCKET + i + j;
					if (slots[idx]) munmap(slots[idx], PAGE);
					slots[idx] = NULL;
				}
				t1 = now_ns();
				per = (t1 - t0) / GROUP;
				if (munmap_min[b] < 0 || per < munmap_min[b]) munmap_min[b] = per;
			}
		}
	}

	for (b = 0; b < nb; b++)
		printf("bucket=%d-%d held=%d mmap_ns=%lld munmap_ns=%lld\n",
		       b * BUCKET, (b + 1) * BUCKET - 1, b * BUCKET,
		       mmap_min[b], munmap_min[b]);

	/* A probe that measured nothing must not score a pass: a clock that
	 * answers 0 makes every bucket 0 and a flat line reads as "no growth",
	 * which is the answer this probe exists to distinguish from. Same rule
	 * `pin_reclaim` and `scripts/mem_suite.py` apply. */
	{
		long long first = mmap_min[0], last = mmap_min[nb - 1], total = 0;
		double slope;
		for (b = 0; b < nb; b++) total += mmap_min[b];
		if (total <= 0) {
			printf("RESULT=INCONCLUSIVE reason=clock_or_mmap_measured_zero failed=%d\n",
			       failed);
			return 2;
		}
		if (failed) {
			printf("RESULT=INCONCLUSIVE reason=mmap_failed count=%d\n", failed);
			return 2;
		}
		slope = (double)(last - first) / (double)((nb - 1) * BUCKET);
		printf("first_bucket_ns=%lld last_bucket_ns=%lld growth=%.2fx "
		       "slope_ns_per_existing_region=%.4f\n",
		       first, last, first ? (double)last / (double)first : 0.0, slope);
		printf("RESULT=OK\n");
	}
	return 0;
}
