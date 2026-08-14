# `MADV_DONTNEED` on a CoW-shared frame zeroed the peer's live page

**Root-caused and fixed 2026-08-14.** This is the mechanism behind **Defect B** —
cargo's heap corrupting under an in-guest `-j4` self-host kernel build, surfacing
as a null `Rc`. Prior investigation:
[`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
§13 (which ruled out PMM-level UAF and narrowed it to "a specific qword corrupted
inside a live page"), and
[`runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)
§"Status" → Defect B for the decode recipe.

Current behaviour lives in
[`reference/subsystems/syscalls/mem.md`](../reference/subsystems/syscalls/mem.md)
→ "`MADV_DONTNEED` — breaks sharing, does not drop the mapping". This document is
the history: what the bug was, how it was proven, and the three method lessons.

## The defect

`sys_madvise`'s `MADV_DONTNEED` arm called `aspace.zero_mapped_page(va)`, which
`memset`s the **physical frame**. That is correct only while the frame has one
holder. After a `fork` it does not: parent and child share the frame CoW, so the
`memset` wrote straight through the peer's live page.

`MADV_FREE` returns `EINVAL` here — deliberately, and correctly
([`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) §5) — and the allocators that
probe it (jemalloc, mimalloc) fall back to `MADV_DONTNEED` on exactly that errno.
cargo forks per rustc invocation, so its heap is full of CoW-shared frames. The
comment in `src/syscall/mem.rs`'s `MADV_FREE` arm had predicted this consequence
in as many words, and the `DONTNEED_SHARED_FRAME` counter existed to catch it.

### What is proven, and what is inferred

Be precise about the seam, because the two halves have very different evidence:

- **Proven, deterministically:** `MADV_DONTNEED` on a CoW-shared frame destroyed
  the peer's page. Measured below, A/B'd, and calibrated against real Linux.
- **Inferred:** that this is the route Defect B took. It is a strong fit —
  nothing else found in this tree writes zeroes into another process's live
  anonymous page, and it explains the audit's otherwise-awkward observation that
  the surrounding page holds **real cargo content rather than zeroes** (the
  advised region is live heap the parent keeps allocating into, so it refills
  around the one pointer field nobody rewrites). But **which** allocator in the
  build reaches `MADV_DONTNEED` was never measured. `jemalloc`/`mimalloc`'s
  `MADV_FREE`→`MADV_DONTNEED` fallback is the named candidate, not a
  confirmed observation; musl's own allocator was measured *not* to call it
  (see "Method lessons" 2).

**To confirm the route rather than infer it**, read `dontneed_shared_frame` out of
the `[MADV]` PSTATS line during an in-guest `-j4` build. Non-zero says the build
genuinely reaches this path; zero would mean Defect B has a second cause and the
hunt is not over. That measurement has not been run — the self-host image was not
required to prove the defect, and the fix does not depend on it.

## The evidence

`userspace/forktest/c_stress/madvshared.c` — allocator-free, deterministic,
milliseconds. It builds the shared frame by hand: `mmap` one page, `memset` a
pattern to fault it in and own it, `fork`, and have one side advise while the
other reads.

```
madvshared on AKUMA (SMP=4)                same binary on real Linux arm64
  child-advises/parent-intact  FAIL          child-advises/parent-intact  PASS
      parent kept 0/4096 bytes                   parent kept 4096/4096 bytes
  parent-advises/child-intact  FAIL          parent-advises/child-intact  PASS
  control/self-zeroed          PASS          control/self-zeroed          PASS
kernel counter: dontneed_shared_frame=2    (exactly the two failing phases)
```

Four things make it airtight, and it is worth reading them together because no
one of them would be enough:

- **The two FAILs are total, not partial.** 0 of 4096 bytes survived — a peer's
  whole page, gone.
- **The control PASSes**, so `madvise` is reaching the kernel and doing its job
  on an unshared page. The FAILs cannot be dismissed as "madvise is a stub".
- **Linux PASSes all three on the identical static binary**, so the probe is
  right and the kernel is wrong (the `futexops` calibration rule).
- **The kernel's own counter reads exactly 2**, agreeing with the probe's two
  failing phases. Instrument and experiment corroborate independently.

That is the null-`Rc` signature verbatim: a live pointer qword in an anonymous
heap zeroed underneath its owner, which safe Rust cannot do to itself.

## The fix

Per page, on `cow_ref` — which counts **address spaces**, first share inserting 2
(`akuma_pmm::cow_ref_inc`), so `>= 2` is the only value meaning "someone else can
see this frame":

| `cow_ref` | Action |
|---|---|
| 0, 1 | zero in place (unchanged; no peer to corrupt, no allocation) |
| ≥ 2 | **break the share** — drop this address space's mapping and reference, install a fresh private zero frame at the same VA |

The share break goes through `unmap_and_free_page` (the `released_last_va` gate,
so `pmm::free_page` routes through `cow_ref_dec` and declines to free while the
peer holds it) and then `map_page(..., RW_NO_EXEC)`.

**Why not Linux's semantics.** Linux drops the *mapping* and lets the next touch
refault, and that was this fix's original plan. It does not survive contact with
this kernel: **eager `mmap`s register no lazy region** (`sys_mmap`, anything
≤ `config::MMAP_EAGER_MAX_PAGES` = 16 pages — including every page `madvshared`
touches and most allocators' small runs), so `ensure_user_page_mapped` would have
nothing to demand-page from and the next touch would be a SIGSEGV instead of a
zero page. Checking that assumption *before* implementing it is the reason this
landed working on the first boot. A private zero frame gives the caller the
identical observable result — mapped, readable, all zeroes — with no dependency
on region bookkeeping, uniformly across eager and lazy mappings.

**Two passes.** Frames must be allocated **outside** `as_lock` (the PMM's reclaim
path re-enters it, and the `Spinlock` is not reentrant), but the state saying how
many are needed is in the page tables. `dontneed_count_shared` counts under the
hold; the allocation happens between the holds; `dontneed_apply` re-reads every
page and acts on what it finds *then*. A peer that broke CoW inside that window
is simply seen as unshared; a page that became shared inside it (a concurrent
`fork`) finds no spare frame and is **skipped and counted**
(`DONTNEED_SHARE_BREAK_SKIPPED`), never wiped. The same skip absorbs a PMM that
cannot serve the batch — the advice is advisory, and failing to zero the caller's
own page beats zeroing someone else's.

## Verification (2026-08-14)

`scripts/verify_trim.py` on the fix vs. a `git worktree` at the parent commit,
same instrument copied into both arms. The entire behavioural diff:

```
< smp1.ex.madvshared: UNEXPECTED           > smp1.ex.madvshared: ok
<   madvshared: 2 FAIL — a peer's live page was zeroed
< smp4.ex.madvshared: UNEXPECTED           > smp4.ex.madvshared: ok
< smp1.passed_marker: 272                  > 273    (the new boot test)
< smp4.passed_marker: 279                  > 280
< smp4.bkl_stuck: 78                       > 77     (load-driven, known noise)
```

Identical on both arms: 4/4 clippy configurations clean, 506 host tests with 0
failures, 95 `[PASS]` at both SMP levels, **empty** failure set on both,
`cowstale` / `bssfork` / `bssfork 20 8 1` / `forkprobe` / `elftest` all ok, and
`host_timejumps: 0` on both — so neither run was starved and both are
trustworthy. Tier 4 (indicated: this is a PMM/CoW-path change) gave
`redis.stage: ok` (= "Your memory passed this test"), `vm_sigsegv: 0`.

Regression coverage, both permanent:

- `madvshared`, now in `verify_trim.py`'s `EXERCISES`, so the gate re-checks it.
- `test_madvise_dontneed_spares_shared_frame` (boot suite) — drives
  `dontneed_count_shared`/`dontneed_apply` against a real `UserAddressSpace` and
  a real CoW-shared frame, asserting the **peer's** 4096 bytes survive, that the
  VA now names a different zeroed frame, that the frame is *not* returned to the
  PMM while the peer holds it, and — the control — that an unshared page beside
  it is still zeroed in place with no frame spent.
- `test_madvise_dontneed_range_semantics` — extended with the
  `dontneed_page_action` truth table.

## Method lessons

1. **The recorded repro was the problem, not the bug.** Defect B's repro is a
   ~1-in-5 crash during a ~15-minute in-guest `-j4` build — roughly 15 clean runs
   to claim "fixed" with any confidence. This tree has already been burned by
   exactly that: a stress repro at that rate once passed **95/96 on BOTH arms of
   a real fix** ([`PMM_EXTRACT.md`](PMM_EXTRACT.md) §8, and the
   `stress_ab_needs_deterministic_probe` lesson). Building an allocator-free
   probe that constructs the shared frame by hand answered the same question
   deterministically, in milliseconds.
2. **Counters alone were not enough either.** A fork-heavy workload of
   `cowstale`/`bssfork`/`forkprobe`/shell pipelines left both `DONTNEED_*`
   counters at **0** — musl's allocator never calls `MADV_DONTNEED`. An
   instrument that never fires does not exonerate the path it watches; it just
   means the workload does not reach it.
3. **Calibrate the probe on real Linux before believing a FAIL.** Every FAIL on
   Linux means the probe is wrong, not the kernel:
   `docker run --rm --platform linux/arm64 -v "$PWD:/w:ro" alpine /w/madvshared`.

## Still open, in the same arm

1. **Unaligned `start`.** Linux rejects a non-page-aligned `start` with `EINVAL`;
   `dontneed_zero_range` rounds it **down**, so the cleared range is a strict
   superset of Linux's and includes the caller's live head page. `DONTNEED_UNALIGNED`
   counts it and has never read non-zero — a separate, unexercised bug that
   deserves its own verification cycle rather than sharing this one.
2. **Unshared `MAP_PRIVATE` file-backed pages.** A *shared* one is now safe (it
   carries a `file_page_cache` reference, so `cow_ref >= 2` routes it to the share
   break). An unshared one is still zeroed in place where Linux would restore the
   file's contents on the next touch
   ([`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) §10, whose `WILLNEED` sibling
   was a real corruption). Wrong, but not cross-process corruption.
3. **`MADV_DONTNEED` still does not return memory to the PMM.** Unchanged from
   before the fix. Doing it properly means giving eager mappings a region first —
   see the reference doc.
4. **The `cow_ref == 1` CoW-break leak.** `complete_cow_break` calls
   `cow_ref_dec(old_pa)` and discards the result, so an address space that breaks
   CoW while it is the *last* holder drops the count to 0 without returning the
   frame. Pre-existing and reached the same way by every `munmap` of a forked
   page; this fix adds one more path that ends there, but no new class.

## Background

- [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
  §13 — the audit that ruled out PMM-level UAF and the per-VA refcount theories.
- [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §5.6 — the refcount-underflow class
  and the `released_last_va` gate this fix reuses.
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) §10 — the `MADV_WILLNEED`
  zero-fill corruption, the same family of bug on the sibling advice value.
- [`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) §5 — why `MADV_FREE` must
  keep returning `EINVAL`, which is what routes allocator traffic here.
