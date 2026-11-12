# MAP_FIXED over an eager mapping kept the old frames and the old record

*2026-11-12, branch `akuma-multiarch-attempt-0`. Found while auditing glue `mem.rs` for mm
convergence step 5 ([`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md)).
AArch64 only; amd64's `mm.rs` and Linux were already right.*

## Symptom

`userspace/forktest/c_stress/mapfixed.c` maps four anonymous pages, writes them, maps four new pages
over them with `MAP_FIXED`, and unmaps. Repeated 2000 times, it shows:

| kernel | free memory lost over 2000 rounds |
|---|---|
| AArch64 (HVF, SMP=4), before | 32 000 KiB: every replaced page (2000 × 4 × 4 KiB) |
| AArch64, after | within noise; `mapfixed: PASS` |
| amd64 (TCG herd VM) | within noise |
| Linux (Lima) | within noise |

The memory came back when the process exited, so short-lived programs never showed it. The
replacement pages read as zero, as they should. Only the frame count was wrong.

## Cause

The `MAP_FIXED` branch of glue's `sys_mmap` cleared the range itself before installing the new
mapping:

- `munmap_lazy_regions_in_range` for the lazy regions, which was right;
- `UserAddressSpace::unmap_page` on every page.

`unmap_page` clears the PTE. It does not free the frame and does not remove it from the
address space's frame ledger, so the ledger held the frame until the address space dropped. The
eager `MmapRegion` that recorded the old mapping also stayed in `Process::mmap_regions`, under
the new region at the same address. That breaks `akuma-mmap`'s invariant that regions never
overlap, which `insert_region_sorted` and `region_index_containing` both rely on. A later
`munmap` of the range detached both records. Only the newer record's pages still had PTEs, so
only its frames were freed.

`sys_munmap` already did all of this correctly. `MAP_FIXED` simply did not use it.

## Fix

`crates/akuma-syscalls-glue/src/mem.rs`: the body of `sys_munmap` became
`unmap_range(proc, addr, len, recycle_va)`. `sys_munmap` calls it with `recycle_va = true`.
`MAP_FIXED` calls it with `recycle_va = false`, since the new mapping occupies the range at
once and returning it to the free list would let a later `mmap` be handed the same addresses.

Removing a mapping that `MAP_FIXED` replaces now does everything `munmap` does:

- detach and clip the eager regions;
- write back a shared file mapping;
- unmap the pages and free the live frames;
- remove the lazy regions;
- clean up stray pages.

## Verification

- `mapfixed` on AArch64 (HVF, SMP=4): `PASS`, no loss over 2000 rounds. `scripts/mem_suite.py`
  runs it as its twelfth probe; 12/12 pass.
- The dynamic loader is the main `MAP_FIXED` user. Checked:
  - `dynchild` returns its designed 42;
  - `dynspawn` passes 800/800 with no divergences;
  - `curl` runs.
- New boot test `test_map_fixed_replaces_eager_mapping`
  (`crates/akuma-kernel-tests-aarch64/src/process_tests.rs`). It runs 8 rounds of fixed map,
  fixed replace and unmap on an activated test process. Each round it checks that one record
  covers the address after the replace and none after the unmap. It also checks the PMM free
  count after draining the UAF quarantine.
  - Passes at SMP 1/2/4.
  - Against a kernel built with the old branch it fails: one more record per round, and 32
    frames lost.
- AArch64 HVF boot suite: 316/325/323 at SMP 1/2/4, the earlier 316/324/322 plus the new test.
  The remaining `FAILED` lines are the thread-ceiling test's `SPAWN FAILED` diagnostic, as before.
- amd64 TCG boot suite at SMP=2: 846 passed, 0 failed. amd64 does not route `mmap` to glue yet.
- Clippy `-D warnings` is clean on AArch64 (five feature sets), amd64 (default, `no-tests`) and
  Taoxichuan (x86_64 and aarch64).

### Writing the boot test

Two traps cost a run each, and any future eager-`mmap` boot test will meet them:

- **The eager install edits only the installed TTBR0.** `map_user_page_tracked_no_flush`
  refuses an address space that is not the one in TTBR0 (its clause 3). A test process that is
  registered but not `activate()`d declines every page (`[MMAP-STALE-PTE] … declined=4`). The
  frames are still allocated and tracked, so the free count drops by every page mapped, which
  looks exactly like the leak under test.
- **Freed frames go to the UAF quarantine** (512 slots) before the free list. Drain it with
  `pmm::quarantine_drain_all()` before each free-count snapshot.

## Open

- **A `MAP_FIXED` into a free-list hole is not removed from the free list.** `MAP_FIXED` now
  leaves the range off the free list when it replaces something. But a fixed mapping placed in a
  hole that an earlier `munmap` returned stays on the list, and `vm_alloc_mmap` can hand the
  same addresses to a later non-fixed `mmap`. That second mapping is then refused page by page
  (`[MMAP-STALE-PTE]`). This was not reproduced; it was found by reading the code. It belongs to
  mm step 5, where amd64's placement cursor and glue's free list meet.
- The `[MMAP-STALE-PTE]` diagnostic resolves `stale_pa` through the installed TTBR0 rather than
  the address space it was refused in. The two are the same on every path except a boot test.

Background: [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) (the region algebra and its
no-overlap invariant); [`CARGO_HEAP_NULL_RC.md`](CARGO_HEAP_NULL_RC.md) D8/D9 (why `munmap`
detaches every overlapping region rather than matching one); [`BOOT_SUITE_PMM_DEFERRED_RECLAIM.md`](BOOT_SUITE_PMM_DEFERRED_RECLAIM.md).
