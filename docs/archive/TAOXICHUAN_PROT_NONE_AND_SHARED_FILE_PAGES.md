# Taoxichuan: `PROT_NONE` and `PROT_READ` were not enforced, and no file page was shared

**Status: FIXED 2026-11-03** in `crates/akuma-tokio-kernel-amd64` (`src/lazy.rs`, the `mmap` and `mprotect` arms, `mount_root`,
`shared_writeback`), with the record bit in `crates/akuma-pte-amd64/src/lazy.rs` and one shared-glue fix in
`crates/akuma-vfs-glue`. These follow [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md), which left all three open.

## Symptoms

- `mprotect(PROT_NONE)` returned 0 and changed nothing. A guard page could be read and written. `mprotectlb`'s
  `rw_to_none_read` and `eager_mprotect_probe`'s PHASE2 failed.
- `mmap(PROT_READ)` mapped the pages read-write, so a write to a read-only mapping succeeded. `mprotect(PROT_READ)` was
  honoured; only `mmap` widened it.
- On a clean `-j3` build of the Infinity closure, `used` peaked at 2.86 GB of 3.14 GB, most of it private copies of
  pages several `rustc`s had mapped from the same files (`librustc_driver` is 311 MB).

## Causes

1. **`PROT_NONE` returned early.** `mprotect_arm` answered 0 for `prot & 7 == 0` before touching a page. The comment gave
   the reason: unmapping the frame would lose the contents a later `mprotect(RW)` has to give back. `mmap(PROT_NONE)`
   mapped the reservation read-write too.
2. **`mmap` ignored `PROT_WRITE`.** Every mapping without `PROT_EXEC` got `(write: true, exec: false)`. That widening
   dates from before this kernel enforced any protection.
3. **Nothing shared a file page.** Each process filled its own frame for every file page it touched. Sharing through
   `akuma-fpcache` (amd64's and AArch64's shared file-page cache) needs every write path to invalidate the cache by
   inode. Otherwise a rewritten file, such as an `.rmeta` cargo regenerates, is served stale to the next `rustc` that
   maps it. That is a silent miscompile. Two write paths did not invalidate:
   - this kernel's own `shared_writeback` (the `msync`/`munmap` write-back of a writable `MAP_SHARED` file mapping);
   - `akuma_vfs_glue::write_at_open_file`, the by-inode write. Its `_or_path` caller invalidated by **path**, which for
     a file unlinked while open resolves to nothing or to another file. amd64's `msync` write-back calls it
     directly and did not invalidate at all. This one is shared glue: on amd64 and AArch64, a `pwrite` to an unlinked
     file left any cached page of it stale for that inode's other mappers.

## Fix

**`PROT_NONE` keeps the frame and clears the user bit.** On a present page, `mprotect(PROT_NONE)` remaps the same frame
with `U/S` clear. To everything else the page is still an ordinary present page:

- ring 3 faults on any access;
- the kernel's own copies refuse it, `translate_filling` through `US`, and glue's `validate_user_range` because
  `x86_user_page_ok` requires `U/S` at every level, so a syscall buffer there is `EFAULT`, as on Linux;
- fork, teardown and the share counts see a present page.

So `mprotect(RW)` gives the contents back. A lazy page's record gets a new bit, `NONE` (bit 5; the inode field moved to
6..=37 and the page index to 38..=63, offsets below 256 GiB), and `lazy::fill` never fills a `none` page. The access
that reached it is a real fault. `mmap(PROT_NONE)` writes `none` records, or, on the eager path, pages without `U/S`.

**`mmap` honours `PROT_READ`.** `write` is `PROT_WRITE`. Writes to a read-only page take the copy-on-write arm's
`Fault` (`akuma-cow`: not writable, not marked), so they are a `SIGSEGV`.

**Read-only file pages are shared.** `lazy::fill`, for a file page whose record is not writable:

- looks it up in `akuma_fpcache` (`lookup_and_ref`, which takes the mapper's count under the cache lock);
- on a miss, fills a private frame, maps it, then `insert`s it, so a peer can map it the moment it lands;
- does not insert a page that runs past EOF, because its zero tail belongs to the mapping, not the file.

A writable private mapping still gets a fresh frame. `mprotect(RW)` on a cached page privatises it first
(`cow_privatize` sees the share count). The cache is armed in `mount_root`, which also registers ext2's inode-freed
hook so a reissued inode number cannot hit an old entry. `shared_writeback` and `write_at_open_file` invalidate by inode
after the write. With the cache on, `alloc_frame` retries once after `akuma_fpcache::shrink(64)`: this kernel has no
reclaim pass that would ask the cache to give back pages nobody maps.

## Verification

TCG, SMP=4, herd as init:

- `protnone` (new, `userspace/forktest/c_stress/protnone.c`) 13/13. It checks:
  - read and write of an untouched `PROT_NONE` page each kill a fork child with SIGSEGV;
  - `mprotect(RW)` then gives a zero page;
  - a written page made `PROT_NONE` faults on read and write;
  - `read(2)` into it and `write(2)` from it are `EFAULT`;
  - `mprotect(PROT_READ)` gives the contents back, and the page still refuses writes.
- `eager_mprotect_probe` PASS (PHASE1 and PHASE2).
- `fpcoherent` (new, `userspace/forktest/c_stress/fpcoherent.c`) 6/6. A page cached by an exited mapper must not
  survive any of these, each followed by a fresh mapping:
  - `write(2)`;
  - `pwrite(2)` to the unlinked file;
  - `ftruncate` to 0 and back;
  - `rename(2)` over the name;
  - a `MAP_SHARED` write-back.
- `fpcpoison /bin/busybox 4 4`: all pass, no page diverged from `read()`. Its gate counter lives in `MAP_SHARED|MAP_ANONYMOUS`
  memory, which this kernel does not share across fork (below), so each round ran un-gated.
- `mmapsum /bin/busybox` (the whole-file digests agree, and so do the two-thread halves), `mmaplazy`, `lazybuf` 8/8,
  `decommitprobe`, `cowstale`, `bssfork`, `mremapmove`, `readback`, `madvshared` and `unixrw` all PASS. A dynamically
  linked `git init/add/commit` and `gcc` work.
- Sharing, measured: six processes each mapping and touching all 4,096 pages of one 16 MB file raised `buff/cache`
  (the cache) by 16.4 MB and left `used` where it was. With private copies the same run costs 96 MB.

Ryzen guest (4 GiB, 4 vCPUs), herd as init:

- ssh works, `echo $PATH` is set, and gcc compiles and runs.
- A clean `-j3` `cargo build` of the **whole** Infinity workspace ran out of **disk**, not memory: `No space left on device`
  at crate 264 of 278, after 698 s. The image is capped near 4 GiB (`INFINITY_ON_TAOXICHUAN.md`, "The disk is capped") and
  the workspace's `target/` reached 1.45 GB. Up to that point `used` peaked at 1.79 GB, plus 0.33 GB of shared file pages
  in `buff/cache`: 2.05 GB in all. That is **not** a like-for-like number against the 2.86 GB of the lazy-mmap run, which
  built four packages (`-p`), on 3 GiB, to the end. The comparison above, of six mappers of one file, is the measured
  effect of sharing.
- No `[Fault]` line on the console.

`mprotectlb` still fails, on a different thing. It catches the fault with a SIGSEGV handler and `siglongjmp`, and this
kernel delivers signals only on a syscall's return: a ring-3 fault ends the task.

## Still open

- **The rerun of the lazy-mmap build** with sharing on: the same four `-p` packages, on 3 GiB, to the end. The guest disk
  has about 185 MB free, so it needs `target/` cleared first, or the bigger disk that triple-indirect ext2 support would
  allow. *(2026-11-03: attempted with `target/` cleared, and the disk ran out before the final link. Peak `used` up to
  that point was 2.27 GB against 2.86 GB, still not like-for-like, and the rerun still needs the bigger disk:
  [`TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md`](TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md).)*
- **Fault signals.** A page fault from ring 3 kills the task (`[Fault] ... task killed`) instead of delivering SIGSEGV
  to a handler. Anything that catches its own faults (GC write barriers, `mprotectlb`) dies.
- **`MAP_SHARED|MAP_ANONYMOUS` is not shared across fork.** Fork copy-on-writes it like a private mapping, so a parent
  and child do not see each other's writes. `fpcpoison`'s gate and every shared-memory counter between processes rely
  on it. *(2026-11-03: fixed, with writable shared file mappings, in
  [`TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md`](TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md).)*
- **A stale page stays in its current mappers.** An invalidation only stops new mappers getting the old frame. A process
  that mapped the file before the write keeps the bytes from before it, as it did with private copies. Linux would
  show it the write. *(2026-11-03: a **writable** `MAP_SHARED` mapper now sees it, through the one-frame table in
  [`TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md`](TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md); a read-only one,
  private or shared, still does not.)*

## Background

- [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md): the lazy record these build on.
- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md): the run whose build these were measured against.
- `docs/reference/subsystems/memory.md`, "Frame lifecycle": the fpcache refcount rule `lookup_and_ref` keeps.
