# File-backed mmap gave every process a private page copy — `-j4` throughput bug

**Status: FIXED 2026-08-05.** A purely mechanical scaling problem, separate
from (and initially confused with) the `-j4` self-host *deadlock* — see
`docs/runbooks/selfhost-kernel-build.md` §5.1a for the maintained summary and
the invalidation-testing gotcha (never stage a test on `/tmp`, which is not
ext2 and so never hits the cache).

## Symptom

`-j4` self-host builds scaled far worse than the job count justified, and
worse than `-j1` throughput would predict. Physical memory ran short mid-build,
`reclaim_clean_file_pages` evicted clean read-only file pages, and each
eviction bought a fresh disk read on the next touch — more jobs meant more
copies, more pressure, more eviction, more I/O, in a loop that `-j1` never
entered because a single copy of the working set fit in RAM.

## Root cause

A demand-page fault on a `LazySource::File` region allocated a **fresh PMM
frame per process** and `read_at`-ed the file bytes into it. Four concurrent
`rustc`s mapping the same toolchain — `librustc_driver.so` (295 MB),
`rust-lld` (154 MB) — held four physical copies of the same read-only text,
filled by four separate ext2 read sweeps.

## Fix

`src/file_page_cache.rs` deduplicates file-backed pages on
`(inode, file_offset)`, so every process mapping the same page of the same
file shares one physical frame, one fill, and one I-cache maintenance pass.
The refcounting reuses the existing CoW refcount mechanism — no new teardown
code was added; `pmm::free_page` already routes through `cow_ref_dec` and
declines to free a frame still referenced elsewhere. Current-state design
(eligibility rules, refcount invariant, kill switch): see
[`../reference/subsystems/memory.md`](../reference/subsystems/memory.md)
§"Shared file pages".

Measured on a 1 GB single-core boot, three concurrent `mmap_file` processes
over the same 8.4 MB file:

| | frames allocated | ext2 page reads |
|---|---|---|
| before (per-process copies) | 3 × 2065 | 3 × 2065 |
| after (`[FPCACHE]` 2065 misses / 4130 hits) | 2065 | 2065 |

Kill switch for a clean A/B: `config::SHARED_FILE_PAGES_ENABLED = false`
restores private copies. Watch the `[FPCACHE]` line in the 30s PSTATS block —
`hits` is exactly the number of private allocations + `read_at` sweeps
avoided.

## Background

- `docs/runbooks/selfhost-kernel-build.md` §5.1a — maintained summary and the
  `/tmp`-is-not-ext2 testing trap.
- [`../reference/subsystems/memory.md`](../reference/subsystems/memory.md)
  §"Shared file pages" — current-state design (this doc is the historical
  record of the fix that section describes).
