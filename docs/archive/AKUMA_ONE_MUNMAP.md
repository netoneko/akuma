# One `munmap` for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. Step 5c of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md).*

## Why

Each kernel had its own `munmap`, and they differed:

| | AArch64 (glue) | amd64 |
|---|---|---|
| records | eager regions detached, then lazy regions, separately | eager regions detached; amd64's demand-paged regions are `lazy: false`, so the same call took them |
| pages | per detached region, by live PTE; a "stray page" sweep of the range **only if no region matched** | one leaf walk of the whole range |
| file pins | released for lazy pieces only | released by `retain_mapped_inode_pins`, a scan per live pin |
| unaligned `addr`, range past the user half | not checked (walked) | `EINVAL` |
| shared writable file pages | glue's `SHARED_FILE_MAPPINGS` | `shmpages`, flushed and reaped in the leaf walk |

The AArch64 shape had one gap of its own: when a range held both a region and pages no region
claimed (a `MAP_FIXED` over part of the ELF image), the stray pages stayed mapped, because the
stray sweep ran only when nothing matched.

## What changed

- **Glue's `unmap_range` is the one body.** Three steps, each in its own lock hold:
  1. write back shared writable pages while mapped (the new `release_shared_write` hook, then
     glue's lazy records);
  2. `akuma_exec::process::detach_regions_in_range`: every region in the range, clipped, in one
     region-lock hold, and the address space's pin dropped for each file no surviving region
     names (a `Vec`, not the 4-slot `OrphanedInodes`: one `munmap` can cross many files);
  3. `UserAddressSpace::unmap_present` sweeps the whole range by live PTE on both walkers
     (AArch64: per page plus one ranged TLB flush; x86: one descent) and hands back the frames
     the ledger says this space held last, freed after the lock is released.
- **`sys_munmap` validates in Linux's order**: alignment, then `USER_VA_END`, then `ESRCH`.
  The `munmap(addr, 0)` one-page divergence is kept.
- **amd64** dispatches `munmap` to glue. `sys_munmap` and the body of `unmap_range` are deleted;
  `unmap_range` forwards to `akuma_syscalls_glue::mem::unmap_current` for `MAP_FIXED`, `mremap`
  and `release_shared_write_mappings`. Its shared writable table is still its own: the hook
  flushes those pages, unmaps them and reaps each table entry whose last mapping it was, before
  glue detaches anything. Step 6 moves that table into `akuma-fpcache-rw`.
- **Gone:** the `MUNMAP-STALE` diagnostic (the record is no longer read for frames at all) and
  the per-region-vs-stray split.

## Pinned

- A page faulted into a shared writable amd64 mapping by another thread between the hook and
  glue's sweep is freed by the sweep without reaping its `shmpages` entry: the entry stays
  referenced by the table alone until exit. The same window existed before (snapshot, then
  detach); it only moved.

## Verification

- clippy, all nine configurations; host tests `akuma-exec` 105, `akuma-mmap` 96, `akuma-mmu`.
- AArch64 HVF boot suite SMP 1/2/4: the same `PASSED`/`FAILED` counts as 5b, all `FAILED` lines
  being `SPAWN FAILED` diagnostics.
- amd64 TCG suite SMP 1/2/4: 822/832/832, 0 failed (two new `munmap` errno checks).
- `mem_suite` 13/13 on the AArch64 devbox (SMP=4) and on amd64 (SMP=2).
- In-VM AArch64 self-host `-j4` kernel build: EXIT=0 in 1m02s; no `PMM-*`, `WILD-DA`,
  `LR-OVERLAP` or `MMAP-STALE-PTE` tripwires.
- Production lines: −32 against 5b, −1 492 against `main`.
