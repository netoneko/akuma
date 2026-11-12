# One VA placer for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. Step 5b of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md).*

## Why

The kernels placed `mmap`s differently:

- **AArch64** used a bump cursor (`ProcessMemory::next_mmap`, CAS-advanced) plus a free list
  (`free_regions`). `munmap` pushed freed ranges onto the list, and first fit took from it before
  bumping.
- **amd64** did a first-fit scan over the sorted region list from a cursor, wrapping once
  (`amd64/src/mm.rs`, 2026-09-17), with no free list.

Moving amd64's `munmap` to glue would have pushed ranges onto a list that amd64's placer never
reads, so the list would grow without bound. Keeping both placers would mean two answers to "is
this range free". The region list already holds that answer.

## What changed

- **The placer.** amd64's scan moved to `akuma_mmap::place::find_free_va_from` (host-tested).
  It gained a `Window { lo, hi, hole }`, so AArch64's kernel RAM identity map
  (`[KERNEL_VA_START, kernel_va_end())`) is skipped exactly as `alloc_mmap` skipped it.
- **AArch64.**
  - `ProcessMemory::place(regions, size)` calls the placer and gains `mmap_base`, the point the
    scan wraps to.
  - `Process::vm_alloc_mmap` places **and records a `PROT_NONE` reservation in one region-lock
    hold**. The mapping then either replaces the reservation (`vm_install_region`, or a lazy push
    via `insert_lazy_region`) or gives it back (`vm_release_reservation`, on every `ENOMEM` after
    placement).
  - `MAP_FIXED` reserves its range the same way after clearing it.
- **Deleted:**
  - `free_regions`, `ProcessMemory::alloc_mmap`/`free_mmap`, `Process::vm_free_mmap`;
  - `unmap_range`'s `recycle_va` parameter;
  - the "recycle a lazy range only if it had frames" rule.
- **aio.** The `io_setup` ring page is now a recorded region. It used to be a mapped page with no
  record, which the region scan would have seen as free.

## Divergences, pinned

| | AArch64 before | now |
|---|---|---|
| reuse of an unmapped range | at once, first fit from the free list | after the cursor wraps |
| a `PROT_NONE` lazy range that was never touched | not recycled, so Go's arena prober would not loop on one address (`test_mmap_lazy_munmap_no_recycle`) | recycled after the wrap. The cursor makes the next `mmap` start past it, so the loop cannot recur |
| two threads placing at once | CAS on the cursor | the region lock, plus the reservation |
| `MAP_FIXED` range between clear and install | not on the free list, so not handed out | reserved |

Five AArch64 boot tests asserted free-list behaviour and were deleted:

- `alloc_mmap_free_region_recycling`
- `arena_trim_crash_pattern`
- `multi_arena_trim_crash`
- `alloc_mmap_free_region_kernel_hole`
- `mmap_lazy_munmap_no_recycle`

The rest call `place(&mut [], ..)`, which with no regions behaves like the bump allocator they
assume. amd64's `va_placement_check` became host tests in `akuma-mmap` (hole cases added). The
cursor/hole/wrap cases for `ProcessMemory` are host tests in `akuma-exec`.

## Verification

- Host: `akuma-mmap` 96, `akuma-exec` 105.
- AArch64 HVF boot suite SMP 1/2/4: 318/326/324 `PASSED` lines, the same as before the change,
  with no failures.
- amd64 TCG suite SMP 1/2/4: 820/830/830, 0 failed. The 17 fewer checks are the placer cases now
  in host tests.
- `mem_suite`: 13/13 on the AArch64 devbox (SMP=4) and on amd64 (SMP=2).
- `thrchurn`, `spawnalias` (22 008 threads, 0 divergences), `clonearg`.
- In-VM AArch64 self-host `-j4` kernel build: EXIT=0 in 1m02s (1m13s on the previous commit). No
  `PMM-*`, `WILD-DA`, `LR-OVERLAP` or `MMAP-STALE-PTE` tripwires.

## Open

- amd64 still uses its own window (`MMAP_BASE..MMAP_TOP`); `ProcessMemory`'s takes over when
  amd64's `mmap` moves to glue.
- The AArch64 `MAP_FIXED` kernel-hole guard (`mmap_fixed_overlaps_kernel_va`) is vacuous on amd64
  but would refuse `[1 GiB, 3 GiB)` there, where amd64's dynamic linker loads (`INTERP_BASE`). It
  has to be windowed per kernel before `mmap` moves.
