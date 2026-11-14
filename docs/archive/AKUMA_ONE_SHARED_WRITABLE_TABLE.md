# One shared writable page table for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. Step 6 of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md).*

## Why

After 5e both kernels ran glue's `mmap`, `munmap`, `mremap` and `madvise`, but a writable
`MAP_SHARED` file mapping still went one of two ways, picked by a `shared_write_backing` hook:

- **amd64** served it from its shared writable page table (`crates/akuma-kernel-amd64/src/shmpages.rs` over
  `akuma-fpcache-rw`): one frame per `(mount, inode, page)` for every mapper, write-back by path or
  by inode, `write(2)` copied into the mapped frames, fork sharing by identity
  ([`AKUMA_AMD64_SHARED_WRITABLE_PAGES.md`](AKUMA_AMD64_SHARED_WRITABLE_PAGES.md)).
- **AArch64** kept glue's `SHARED_FILE_MAPPINGS`: a `(tgid, base) → (path, offset, len)` record,
  private frames, write-back of the present pages at `msync`/`munmap`/exit. Two processes mapping
  the same file never saw each other's stores, and neither did a forked child. Every `shm*` probe
  failed on AArch64 (`shmwrite` 11 rungs, `shmcoh`, `shmshare` 4/8, `shmvar`, `shmregionprobe`).

## What changed

- **The table moved to `crates/akuma-exec/src/shm.rs`**, unchanged in substance: `fault_page`
  (hit, or fill from the file and publish), `map_existing` (a reader maps a writer's frame),
  `reap`, the `MappedFileHooks` (`active`/`wrote`/`zeroed`) and `inode_freed`. `akuma-exec` is
  `forbid(unsafe_code)` and keeps it; the fill reads through the runtime's `read_at_by_inode`,
  which both kernels register. `akuma_vfs_glue::init_shared_write` registers the hooks for both.
- **Glue owns write-back and release.** `snapshot_shared_write` + `flush_shared_write` (the
  file's size through the fd's inode, then by path or by inode, per present page),
  `release_shared_write` (flush, `unmap_present`, then `shm::reap_released` for exactly the frames
  that unmap released), `release_shared_write_mappings` for exit and `execve`, and one `sys_msync`.
  `mmap` builds the `shared_write` record from the fd; a fd with no inode is `EACCES`.
- **Fork** shares `shared_write` regions by identity on both kernels (`region_ranges` in
  `akuma-exec`'s fork) and `inherit_mmap_regions_for_cow_child` copies the record, so amd64's
  fork fix-up that re-attached it went.
- **AArch64's fault path** (`demand_page_lazy_region`) asks `shm::region_key` and then calls
  `fault_page` for a `shared_write` page, or `map_existing` for a non-writable page of any other
  file mapping. When the mapping is executable it runs `sync_icache_range` on the frame, as the
  private path would.

**Deleted:** `crates/akuma-kernel-amd64/src/shmpages.rs`; amd64's `sys_msync` (and dispatch's nr-26 pre-route),
`shared_write_backing`, `release_shared_write`, `release_shared_write_mappings`,
`WriteBackRegion`, `snapshot_shared_write`, `flush_shared_write`, `write_back_target_ok` and
`fd::file_write_identity`; glue's `SharedFileMapping`, `SHARED_FILE_MAPPINGS`,
`writeback_shared_pages`, `flush_and_clear_shared_file_mappings` and the eager shared-writable
path; the two glue hooks and their registrations in both kernels and Taoxichuan's runtime; the
AArch64 boot test `test_shared_file_mmap_writeback`, which tested the deleted record.

## Two bugs found on the way to green

1. **`PROT_READ` is executable on AArch64.** `Prot::RO` encodes as `AP_RO_ALL` with no `UXN`, so
   `user_flags::is_exec` is true for it. The first version skipped the table for executable
   mappings (to avoid I-cache maintenance on the table's frame), which skipped it for every
   read-only mapping: `shmregionprobe`'s "parent's read-only map sees the bytes" kept failing.
   Fixed by taking the table for every non-writable mapping and doing the maintenance after an
   executable one is installed.
2. **The page straddling EOF.** `region_key` returned a key only for a page wholly inside the
   file's data, a rule copied from the read-only file-page cache, where it stops a past-EOF tail
   being published. `shmregionprobe` maps 96 pages and 360 bytes, so the reader's last page read
   the file's stale zeros. amd64's fault path had always asked the table for every page. Now
   `region_key` does too.

Both were caught by the AArch64 `shmregionprobe` run; a temporary trace in the fault hook showed
96 hits and one `region_key` miss at the last page.

## Verification

- clippy, all nine configurations; host tests.
- AArch64 HVF boot suite SMP 1/2/4: 313/321/319 `PASSED`. The only difference from 5e is the deleted
  `shared_file_mmap_writeback` test (diffed by name). amd64 TCG suite SMP 1/2/4: 822/832/832, 0
  failed.
- AArch64 devbox (SMP=4): `shmwrite` 0 failing, `shmcoh` both `YES`, `shmshare` 8/8, `shmvar` all
  pass, `shmregionprobe` all ok, `mapnoreplace` all pass, `mem_suite` 13/13. All of the `shm*`
  probes failed there before this step. `altstackexec` still fails as on 5d and 5e (AArch64 signal
  delivery, not mm).
- amd64 (SMP=2): the same seven probes all pass, `altstackexec` included; `mem_suite` 13/13.
- AArch64 devbox self-host `-j4`: EXIT=0 in 1m09s, no `PMM-*`/`WILD-DA`/`LR-OVERLAP`/
  `MMAP-STALE-PTE`/panic lines.
- amd64 self-host on the ryzen (Firecracker guest over p3, `kbuild -c -j 4 --features no-tests`):
  EXIT=0, 127 crates. Console: one benign `[MM] fault race`, 8 `[BKL] stuck` lines, no panics or
  tripwires.

## Correction, 2026-11-12 (same day): write-back undid peers' stores

`shmwrite` rung 10 (4 processes x 50,000 atomic increments) lost 10-25 % of them in 3 of 5
runs on the next kernel built; the step-6 kernel above passed it 6 of 6, by timing. Glue's
`flush_shared_write` copied each page into a buffer and wrote the buffer. The write reaches
`shm::wrote`, which copies a file write into the table's frame unless the write's source *is*
that frame. A buffer never is. So an exiting child's flush stored its snapshot back over the
shared page, undoing every increment its peers had made since. amd64's own flush, before this
step, wrote from the frame's kernel mapping; the move to glue lost that. The flush now writes
straight from the frame (`with_phys_bytes_mut`), so `wrote` skips it. Rung 10: 6 of 6 after
the fix, AArch64 devbox. It is the same code on amd64.

## Open

- A table entry left by a read-only mapper that never shares the frame with a writer again stays
  until its inode is freed; nothing sweeps it (a sweep could drop a killed writer's unflushed stores).
- A process killed with unflushed table pages leaves them unflushed until another mapper flushes
  or the inode goes.
- A reader that faulted a page before the writer's first fault keeps the file's older bytes until
  it faults again (`shmregionprobe`'s first info line). Linux passes; Chromium does not do this.
- `read(2)` still does not see a mapping's unflushed stores.
- Taoxichuan keeps its own copy (`crates/akuma-tokio-kernel-core/src/shmpages.rs`).
- `brk` is still per kernel (5e's open item).
