# One `mremap` and one `madvise` for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. Step 5e of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md).*

## Why

After 5d, both kernels kept their own `mremap` and `madvise`, over the same region list:

- **amd64's `mremap` moved pages.** It re-pointed each present leaf at the new address, carried
  the region's `file`, `shared_write` and `shared_anon` records, and refused `MREMAP_FIXED`.
- **AArch64's `mremap` copied pages.** It allocated `new_pages` fresh frames and copied through a
  bounce buffer, ignored `MREMAP_FIXED`, and kept only the protection. That shape cost two bugs
  before: a copy loop that stopped at the first lazy page and silently truncated the mapping
  (`USER_COPY_FOLD.md` §5, the reason `mremapmove` exists), and `unwrap_or(NONE)` killing `rustc`
  (`GRANT_RECORDS_VS_DENY_RECORDS.md`).
- **The two `MADV_DONTNEED`s disagreed on what to touch.** amd64 touched only pages inside a
  recorded region, skipped device pages, wrote back and dropped shared writable table pages, and
  mapped a replacement frame with the region's protection. AArch64 touched any mapped page and
  mapped a replacement `RW_NO_EXEC`, even in a `PROT_NONE` region.

## What changed

**`mremap`** is glue's, in amd64's design:

- `MREMAP_FIXED` → `EINVAL`.
- The new region is `MmapRegion::demand_paged` with the old region's recorded protection, and
  its `file`, `shared_write` and `shared_anon` records advanced to `old_addr`'s place.
- `UserAddressSpace::move_present(start, end, new_base)` on both walkers moves each present leaf
  with its bits. AArch64 collects and clears the source entries, flushes, then maps each frame at
  the new VA. x86 does the same through `rewrite_leaves_in_range`, keeping a device page's cache
  type. If it runs out of table frames partway, what moved is mapped and the rest demand-pages.
- `unmap_range` retires the source; glue's `SHARED_FILE_MAPPINGS` record is re-keyed to the new base.
- With no process, a growth that may move is `ESRCH`; one that may not is `EFAULT`.

**`madvise`** is glue's. `MADV_DONTNEED` keeps glue's two-pass break-sharing algorithm
(`MADV_DONTNEED_SHARED_FRAME.md`) and takes amd64's rules about *what* it touches:

- Regions are walked a batch of 16 at a time under the region lock. Pages outside every region and
  outside the `brk` heap are left alone. The batch is a stack array, which also removed the old
  per-page `Vec` that reserved `pages` words before the lock.
- A lazy file span drops its pages, 64 at a time from a stack array, for the next touch to re-read.
- An anonymous span runs the two passes, with the region's protection passed into
  `dontneed_apply` for the replacement frame.
- `ram_frame` filters out anything `akuma_pmm::contains` does not vouch for, so `/dev/fb0` is not
  zeroed.
- The `release_shared_write` hook runs first: on amd64 it writes back and drops shared writable
  table pages, and on AArch64 it is a no-op until step 6.
- No process → `ESRCH`, as amd64 answered. The AArch64 boot test that expected `0` with no process
  now asserts only that the range guard was passed.

**Deleted on amd64:** `sys_mremap`, `sys_madvise`, `dontneed_range`, `DontneedFlush`,
`DontneedRelease`, `ZERO_PAGE` and `find_free_va`. The boot-suite errno checks (`mremap_check`,
`madvise_check`) now drive glue through `to_glue`.

## Divergences, pinned

| | AArch64 before | now (both) |
|---|---|---|
| `mremap` with `MREMAP_FIXED` | flag ignored, address chosen | `EINVAL` |
| `mremap` of a file mapping | new pages anonymous copies; unfaulted pages came back as zeros | file record carried; unfaulted pages read the file |
| `MADV_DONTNEED` on a page in no region (ELF image) | zeroed | untouched |
| `MADV_DONTNEED` with no process | `0` | `ESRCH` |
| replacement frame in a `PROT_NONE`/`PROT_READ` region | `RW_NO_EXEC` | the region's protection |

`brk` heap pages are in no region but are still advised, since a heap allocator may expect zeros.

## Verification

- clippy, all nine configurations; host tests.
- AArch64 HVF boot suite SMP 1/2/4: 314/322/320 `PASSED`, no real failures (the `FAILED` lines are
  `SPAWN FAILED` diagnostics), the same as 5d.
- amd64 TCG suite SMP 1/2/4: 822/832/832, 0 failed.
- `mem_suite` 13/13 on the AArch64 devbox (SMP=4) and on amd64 (SMP=2), `mremapmove` and
  `madvshared` included.
- amd64: `shmwrite` (0 failing), `shmcoh`, `shmshare` 8/8, `shmvar`, `shmregionprobe`,
  `mapnoreplace`, `altstackexec` — all pass.
- AArch64: `shm*` logs byte-identical to 5d (they fail on both; that is step 6), `mapnoreplace` passes.
  `altstackexec` fails identically on the 5d kernel: the exec'd-image trap and the post-churn
  pthread trap do not run the handler. That is an AArch64 signal-delivery gap, not mm, and is open.
- AArch64 devbox self-host `-j4`: EXIT=0 in 1m05s, no `PMM-*`/`WILD-DA`/`LR-OVERLAP`/
  `MMAP-STALE-PTE` tripwires.
- amd64 self-host on the ryzen (Firecracker guest over p3, `kbuild -c -j 4 --features no-tests`):
  EXIT=0, 127 crates. Console: one benign `[MM] fault race`, 10 `[BKL] stuck` lines, no panics or
  tripwires.

## Open

- `msync` and `brk` are still per kernel. amd64's `msync` flushes its shared writable table, which
  is step 6. amd64 never grows `brk`, and glue's `set_brk` maps pages with no region record, no
  overlap check against mappings and no failure on OOM; routing amd64 to it needs those first.
- `MADV_WILLNEED` now pre-faults anonymous lazy pages on amd64 too (it was a no-op there).
