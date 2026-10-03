# amd64: shared writable file pages — SQLite's multi-process WAL, and the worker-thread fork under it

**2026-10-03.** Writable `MAP_SHARED` file mappings on amd64 were per-process copies with
write-back, so SQLite's WAL index (`-shm`) diverged between processes and goose's
`sessions.db` came back `database disk image is malformed`. Fixed with a shared writable page
table: **one frame per `(mount, inode, page)` for every mapper**. Fixing it exposed a second,
older bug: **`fork` from a worker thread read the thread's own empty region list**. With the new
table that bug destroyed the shared index on every goose tool call. Both are fixed and verified
on QEMU and on the trashcan's metal, and six concurrent long goose sessions now leave
`integrity_check` `ok`.

Earlier history: `AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md` § 9 (`F_GETLK`), § 14 (real record
locks) and § 15 (the coherence diagnosis this doc resolves). The design reference is
[`../reference/subsystems/amd64-shared-write-mmap.md`](../reference/subsystems/amd64-shared-write-mmap.md).

## 1. The problem, as it stood

- Writable `MAP_SHARED` on a file (2026-09-22, for parity-db) was **demand-paged fills plus
  whole-region write-back** on `munmap`/`msync`/`MADV_DONTNEED`. There was no page cache, so
  each mapper had private frames.
- SQLite's WAL index is a 32 KiB `MAP_SHARED` mapping of `<db>-shm` that every connection in every
  process must see live. `userspace/forktest/c_stress/shmcoh.c` printed `NO`/`NO`, and
  `scripts/benchmarks/sqlite_wal_stress.py procs 4 40` ended `malformed` even with § 14's record
  locks in place.

## 2. Which fix, and why not the `-shm` special case

§ 15 offered three options. Option 1, the general design, was chosen, after checking it against
the code:

- **The pieces already existed.** `akuma-fpcache` keys frames by `(inode, mount, offset)` and
  counts them through the PMM's CoW refcount. `fork` already shares `MAP_SHARED|MAP_ANONYMOUS`
  by identity: same frame, writable in both, never CoW-marked. The work was a *writable* table
  with different lifetime rules, plus teaching each mapping operation about it.
- **The special case would not have been smaller.** Kernel-global frames keyed by path still need
  the same fault, fork, `mprotect`, `MADV_DONTNEED`, `mremap` and teardown handling. Every item
  in § 3 below applies to it too. It would only have saved the write-back, and it would have fixed
  SQLite while leaving every other multi-process `MAP_SHARED` user (parity-db across processes,
  anything `memmap2`-based) incoherent.

`akuma-fpcache` itself could not just be widened. Three of its rules are wrong for writable pages:

- **Eviction and invalidation.** Dropping a live entry strands a mapper on an orphan frame.
- **The lost-race loser stays private.** That means two copies.
- **`write(2)` invalidates.** That would drop stores the file has not seen yet.

## 3. The mechanism

**`crates/akuma-fpcache-rw`** (new, `#![forbid(unsafe_code)]`, empty `[dependencies]`, 14 host
tests) holds the decisions:

- `Table` with `lookup_and_ref` / `publish` / `reap` / `for_each_in` / `detach_inode`.
- `write_span` / `zero_span` (where a file write lands in a page).
- `Generations` (the fill race).
- `marks_cow` (the CoW-marking rule plus its identity-sharing exception).

The kernel half is **`amd64/src/shmpages.rs`**: one IRQ-masked spinlock, the PMM refcount and
the physmap copies. Lock order is address space → table → `COW_REFCOUNTS`/PMM.

| path | what it does now |
|---|---|
| fault (`mm::fill_shared_write_pages`) | A lazy region carrying a `shared_write` record looks the page up. A hit maps that frame RW and un-marked. A miss reads the file's **current** page (not clamped by the `mmap`-time `filesz`) and publishes it. A lost publish race adopts the winner's frame. |
| fill race | A `write(2)` landing between the fill's read and its publish would otherwise leave a stale page. Per-inode-bucket generations are bumped by every write-through before it takes the lock and checked by `publish` under the lock, so a raced fill re-reads (bounded at 8 attempts). |
| `write(2)`/`pwrite`/`ftruncate`/`fallocate` | New `akuma_vfs_glue::MappedFileHooks` (no-op unless a kernel registers them, so AArch64 is untouched). Writes are **copied into** mapped frames; truncate and hole punch **zero** them. A write whose source *is* the frame (`munmap`'s write-back hands `write_at` a physmap slice) is skipped: copying it onto itself would undo a peer's concurrent store. |
| `munmap` | Flush first (unchanged). After dropping this mapping's reference, `reap` the entry if no mapper is left. The flush has already put the bytes in the file, which is now the page's only copy. |
| exit and `execve` | New `mm::release_shared_write_mappings` gives them `munmap`'s treatment. Before, the address space's `Drop` freed the frames with **no flush**, so a process killed with a live mapping lost writes. Now `shmwrite` rung 11 passes. |
| `fork` | Shared-writable regions join the shared-by-identity ranges. The child re-gets the `shared_write` record, which the shared crate drops for AArch64's copying fork. The share arm now takes one reference **per frame** (`adopt_user_frame`), not per VA. |
| `MADV_DONTNEED` | On a table page, drop this mapping and keep the page, as Linux does. It used to zero the frame, which now meant zeroing it for every mapper. |
| `mprotect` | Never CoW-marks a page of a shared-by-identity region (`marks_cow`). This also fixes a **pre-existing** bug: `mprotect(RO)` then `mprotect(RW)` over a forked `MAP_SHARED|MAP_ANONYMOUS` page re-marked it, and the next write split parent from child. |
| `mremap` | The new region carries `file`, `shared_write` and `shared_anon`. **Also pre-existing:** a moved lazy file mapping lost its `file` record, so its unfaulted pages came back as anonymous zeros. |
| write-back target | A flush first checks that the recorded path still resolves to the mapping's `(mount, inode)`. SQLite unlinks `-shm` *before* unmapping it. And after an unlink-and-recreate, the old flush used to write into **the new file**. |
| inode freed | ext2's hook is now `shmpages::inode_freed`, which runs fpcache's invalidation and then drops any table leftovers. |

**Allocation.** The table is a `BTreeMap`, so publishing may allocate a node on the fault path,
infallibly. That is the cost `akuma-fpcache` already pays, and it is bounded by residency, since
entries exist only while mapped. The write-through and zeroing copy into frames with no
allocation. The fill reads straight into the frame, with no readahead buffer. `flush_shared_write`
used to clone the path `String` once per flushed page, and that flush now runs on every exit; it
uses a region index instead.

## 4. The second bug: `fork` from a worker thread

With § 3 in, `shmcoh`, the 13-rung `shmwrite` probe and `sqlite_wal_stress.py procs` all passed
on the metal. **goose still failed.** Two concurrent long sessions gave
`(code: 779) database disk image is malformed`, then `(code: 11)`, while `integrity_check`
afterwards still read `ok`. These are transient stale reads inside a live process.

**How it was cornered, cheapest experiment first:**

1. *Does goose mmap the database file?* No. Its bundled SQLite is built `MAX_MMAP_SIZE=0` (the
   compile-option strings are in the binary). The only shared mapping is `-shm`.
2. *Does `fd.rs` cache file contents?* No (since C2 slice 5).
3. *Does a fork child share the parent's record-lock owner?* No. `fork_process` and the
   `posix_spawn` vfork (served as a fork) deep-copy the fd table into a new `Arc`.
4. **A `spawn` mode in `sqlite_wal_stress.py`** (`mixed` plus `sh -c true` started from inside an
   open write transaction, because every goose tool call is a spawn) reproduced it **with real
   on-disk corruption** (`Rowid … out of order`, `wrong # of entries in index`).
   - `spawn 1` (one process): clean.
   - `procs`/`mixed` (no spawns): clean.
   - Only many processes *and* spawns corrupt.
5. A `TEMP-DIAG` print in `shmpages::wrote` showed **whole-page write-throughs, 8 × 4096 bytes:
   the whole `-shm`**. Some process was writing back an index from a frame that was not the
   table's. That is a private copy, and the write-through then stamped it over everyone's live
   index.

**Root cause.** `usermode.rs::share_parent_memory_into` read `parent.mmap_regions`, and `parent`
is the **forking thread's** `Process`. A `CLONE_THREAD` thread's own region list is empty by
construction: memory belongs to the leader, which is what `current_mm_process` resolves for
every other memory syscall. Python's `subprocess`, like tokio's `spawn_blocking` running a
`Command`, forks from a worker thread. So:

- the child inherited **no regions**, and any lazy page it touched before `exec` was "unmapped";
- there were **no shared ranges**, so every `MAP_SHARED` page was CoW-demoted in the parent. The
  parent's next write then copied the page and left the sharing.

Before § 3 that was invisible for `-shm` (nothing was coherent anyway), and for
`MAP_SHARED|MAP_ANONYMOUS` it was a silent split nobody had probed from a thread. After § 3 the
private copy's write-back wrote a stale index into every other process's frame.

**Fix.** Read the owner's region list; it is the same `owner` the function already locked for the
page walk. **A/B on local QEMU:** `shmwrite` rung 13 (fork from a worker thread; the parent must
stay shared with an unrelated mapper, and the child must see a never-faulted page) **FAILS** with
the one line reverted and **PASSES** with it.

## 5. Measured

**Local QEMU** (`scripts/utils/amd64_shmwrite_check.py`, new; plus the existing gates):

- Boot suite: `790 passed, 0 failed`, unchanged from the stock count.
- `amd64_mem_trials.py --local-only`: 11/11 (`smapsdirty`'s two `DIVERGE` are its usual ones).
- `amd64_ring3_check.py -n 20`: OK.
- `shmcoh`, `shmwrite` (13/13), `fcntl_lock`, `unixsock_amd64`, `groupexit_thread` ×3 modes,
  `shmanon`, `madvshared`, `mremapmove`: all pass at SMP=1, and at SMP=4 three times over in one
  boot (`shmwrite` rung 10: 4 processes × 50 000 atomic increments = exactly 200 000).

**The trashcan, bare metal, `--features no-tests`:**

- The same probe set, plus `grandfork` and `sigprobe`, all pass.
- `sqlite_wal_stress.py`, all `integrity ok`:
  - `threads 6 40`
  - `procs 4 40`, `procs 8 90`, `procs 4 60` ×2
  - `mixed 4 40` ×3
  - `spawn 2/3/4 40`, after the fork fix (corrupt before it)
  - `spawn 1 40` ×2
- goose:
  - 3 sequential runs, then 3 concurrent short pairs: all exit 0, integrity ok.
  - **3 concurrent long tool-heavy sessions, twice** (17 + 10 + 10 shell calls each round): all
    six exit 0, integrity ok, 6 sessions / 166 messages.
  - Before the fork fix, the same shape failed in 2 of 2 tries.
- Leak check: free memory ended 1.6 MB higher after a stress round and three `shmwrite` runs. No
  `[MM-WB]` failures, no `[PMM-RESURRECT]`.

## 6. Not done / still open

- **`read(2)` does not see a mapping's unflushed stores.** The write-through goes file → frames,
  never back. SQLite does not need it (it never `read`s `-shm`); `shmwrite` rung 4 checks only
  after `msync`.
- **A `PROT_READ`-only `MAP_SHARED`** of a file still goes through the read-only `akuma-fpcache`
  path, and so does `MAP_PRIVATE`. Either can hold a page older than a peer's shared-writable
  frame. SQLite with `mmap_size > 0` maps the *main DB* exactly this way. goose's build cannot
  (`MAX_MMAP_SIZE=0`); other SQLite builds can.
- **A rename under a live mapping** now makes the flush skip, where it used to write to a
  now-wrong path. The bytes stay correct in the shared frames while mapped, but they are lost when
  the last mapper goes. A by-inode write would fix it; `write_at_by_inode` does not exist.
- **The eager path is unchanged.** A shared-writable mapping whose fd has no inode identity still
  gets private frames.
- **SQLite busy-wait starvation (separate finding).** In `mixed`/`spawn` with ≥ 3 processes × 3
  threads, roughly one connection in twelve times out at its first `BEGIN IMMEDIATE` after the
  full 30 s busy timeout: `database is locked`, never corruption. The same load on macOS never
  times out and runs about 18× the throughput (≈ 120 transactions/s on the box). This is SQLite's
  sleep-and-retry busy handler meeting a lock that is almost always held. Not investigated: where
  the per-transaction time goes.
- **AArch64 not touched.** It keeps its own `SharedFileMapping` (`akuma-syscalls-glue/src/mem.rs`)
  and its copying fork, and `mmap::plan` did not change. **Whether AArch64's `fork_process`
  (`akuma-exec` `process/mod.rs`, `inherit_mmap_regions_for_cow_child(&parent.mmap_regions…)`) has
  the same worker-thread blind spot was not checked.**
- The `idt.rs` kill-path diagnostic still reads `current_process()`'s own region list, so for a
  thread it can claim `cr2` was outside every region. Cosmetic.
- `docs/reference/crate-safety.md` was not regenerated for the new crate (its numbers are already
  older than several crates).
- An *interactive* goose session was not driven. The concurrent long `goose run` sessions above
  are the multi-process evidence.

## Files

- **New:** `crates/akuma-fpcache-rw/`, `amd64/src/shmpages.rs`,
  `userspace/forktest/c_stress/shmwrite.c`, `scripts/utils/amd64_shmwrite_check.py`.
- **Changed:** `amd64/src/{mm,usermode,fs,idt,main}.rs`, `amd64/Cargo.toml`, `Cargo.toml`,
  `crates/akuma-vfs-glue/src/lib.rs` (the hooks), `crates/akuma-mmap/src/region.rs` (docs only),
  `scripts/benchmarks/sqlite_wal_stress.py` (`mixed`/`spawn` modes, error timing, close on error).

## Background

- [`AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md`](AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md) § 9, § 14, § 15
- [`AKUMA_AMD64_SHARED_WRITE_MMAP.md`](AKUMA_AMD64_SHARED_WRITE_MMAP.md) — the 2026-09-22 write-back design this builds on
- [`MAPPED_PAGE_PREMATURE_FREE_FIX.md`](MAPPED_PAGE_PREMATURE_FREE_FIX.md) — `akuma-fpcache`'s W1/W2 refcount races, whose rules the new table follows
- [`AKUMA_AMD64_RING3_SEAM_SLICE7.md`](AKUMA_AMD64_RING3_SEAM_SLICE7.md) — the `clone` fold, and why a thread's own `Process` holds no memory (`current_mm_process`)
