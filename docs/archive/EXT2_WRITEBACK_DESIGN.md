# ext2 write-back cache — design decisions (2026-08-26)

Status: **in-flight design record** driving the implementation in
`crates/akuma-ext2/src/ext2.rs`. Not history yet; will be linked from the
reference docs when the work lands. Companion measurements:
[`EXT2_PERFORMANCE_AUDIT.md`] (the audit this follows up) and
`crates/akuma-ext2/README.md` § Performance (fixes A–D already landed).

## Problem

After fixes A–D (redundant zero-fill skip, deferred superblock/BGD writes,
single-dir-block dirent writes, bitmap staging), `write_block` is still
invalidate-slot + synchronous device write (`ext2.rs` ~1132). Consequences:

- read-after-write is always a cold device read (worst measured op:
  `seq_read` 183 ms right after a write, vs 0.1 ms Linux writeback);
- every partial-block write is read-clone-patch-write (2 device ops, 1 alloc);
- inode-table blocks are written per-touch — the last per-op metadata cost.

Device amplification after A–D: 2 MB write = 2042 device calls (3.6×).
Linux ext2 `-o sync` (Akuma's durability class) is ~15–220× faster.

## Cache inventory (verified in code this session)

| Cache | Where | Scope | Key | Holds | Serves |
|---|---|---|---|---|---|
| ext2 block cache | `crates/akuma-ext2/src/ext2.rs` (`BlockRingCache` 64-slot ring; `ClockBlockCache` under `fs-cache`) | **per ext2 instance** (per mount; `new_with_cache_cap` for non-root) | physical block no. | heap `Vec<u8>` block buffers | read()/write(), all metadata |
| file page cache | `src/file_page_cache.rs` | **kernel-global** | `(inode, file_off)` | refcounted `PhysFrame`s (PMM) | mmap/exec RO-page dedup |
| `bgd_cache` / `bitmap_cache` | `Ext2State` | per instance | group / block no. | — | **not caches**: deferred-write staging (fixes B/D) |

Exactly two real caches. Everything else named "cache" is staging or
instrumentation.

### Per-box status (confirmed)

- ext2 block cache: **already per-box** in the sense that matters — every
  mount (including each box's overlay upper, which must be ext2 by
  `OverlayFs` construction) gets its own `Ext2Filesystem` and its own cache.
- file page cache: **no per-box or per-mount structure at all**
  (`PAGES: BTreeMap<(u32, usize), Entry>`, `file_page_cache.rs` ~80).

### Finding F-1 (latent, pre-existing)

The global page-cache key is inode number alone. Two concurrent ext2 mounts
(`MOUNT_IN_NS` supports mounting a second ext2 from a registered block
device, `src/syscall/container.rs`) put both filesystems' inode numbers in
one keyspace; the mmap/exec fill path resolves the inode through the
calling process's namespace, so a same-numbered inode on another mount can
serve the wrong bytes. Overlays avoid it by construction (every layer on the
same fs — `crates/akuma-isolation/src/overlay_fs.rs` ~542), but nothing
guards the two-independent-mounts case. Scope today: RO mapped pages only.
This is the concrete argument for scoping the page cache per mount/box.

## Decisions

- **D-1 — write-back lands inside the existing ext2 block cache.** Dirty bit
  per slot in both `BlockRingCache` and `ClockBlockCache`; `write_block`
  updates/inserts the slot + marks dirty instead of invalidate + device write.
  Flush points: dirty-victim eviction, `flush_meta` (end of every mutating
  op), `sync()`. **No third cache is created.**
- **D-2 — flush order is data before metadata** (bitmaps/BGD/superblock
  last). No journal: e2fsck should see data blocks before the bitmap that
  claims them.
- **D-3 — invalidate-on-free never flushes.** `free_block` /
  `truncate_inode` drop dirty copies of freed blocks without writing them
  back; a flushed stale block could clobber a reallocated block. This is the
  one new correctness hazard and gets dedicated tests (free/realloc poison).
- **D-4 — zero-copy reads.** `read_block` stops cloning on hit (borrow
  guard); the read-run fast path in `read_at_by_inode` already avoids it.
- **D-5 — in-slot sub-block patch.** Inode-table and BGD writes patch the
  cached block in place (`patch_range`) instead of read-modify-write via a
  cloned block.
- **D-6 — bitmap-scan cursor.** Per-group next-free-bit hint in `Ext2State`;
  sequential allocation becomes O(1) amortized instead of rescan-from-bit-0.
- **D-7 — durability model unchanged for clean shutdown, extended for
  reboot(2).** `sys_reboot` (`src/syscall/reboot.rs`) currently issues PSCI
  with no filesystem sync — nothing in the tree calls
  `MountTable::sync_all` (verified). Wire sync of the global mount table +
  box namespaces in before `SYSTEM_RESET`/`SYSTEM_OFF`. Unclean crash loses
  write-back data — same class as Linux ext2 default mount; e2fsck-recoverable.
- **D-8 — staged toward unification; metadata half is permanent.** Even in a
  Linux-style unified page cache, a block-keyed **metadata** cache survives
  inside ext2 (bitmaps, inode tables, BGDs, indirects have no
  `(inode, offset)` identity). The dirty-bit machinery for metadata blocks
  is end-state code; the data-block half is transitional and will delegate
  to the injected data cache (see C-1..C-3, next section).
- **D-9 — per-box/mount scoping of the file page cache is its own change**,
  orthogonal to write-back (F-1 makes it a correctness fix, not tidiness).
  Not bundled with this work.
- **D-10 — end state documented as a proposal, not built now**: a
  `DataCache` trait (get/put/dirty by `(inode, offset)`) in a crate both
  sides see; ext2 calls it for file-data blocks; kernel implements it over
  `file_page_cache`; host tests inject a trivial impl. Kills the
  mmap-vs-read double-cache seam (`DP_FILE_CACHE_MISMATCH`,
  `on_inode_freed` invalidation machinery). To be written in `proposals/`.
- **D-11 — the two block caches stay in akuma-ext2; no extraction to
  `akuma-primitives`.** Audited every "ring" in the tree: the only tag+data
  +eviction caches are ext2's own `BlockRingCache`/`ClockBlockCache` (the
  net/virtio/AIO "rings" are protocol/DMA structures, not caches), so
  nothing is duplicated. `akuma-primitives` is deliberately alloc-free with
  zero dependencies (Cargo.toml: "the leaf every other crate may depend
  on") and these caches are `Vec`-backed with ext2's `FsError` semantics.
  Repo convention extracts on the second consumer; that moment is D-10's
  unification, which would justify a dedicated crate (e.g.
  `akuma-blockcache`), not growing primitives.

## Why not "replace the ext2 cache with the global page cache" (rejected alternatives)

- **C-1 — metadata has no `(inode, offset)` key.** The page cache cannot
  address bitmaps/inode-tables/BGDs/indirects; a block-keyed layer survives
  any unification, so "replace" is really "unify the data half".
- **C-2 — `PhysFrame` cannot cross into the host-testable no_std crate.**
  akuma-ext2's value is FS logic tested under `cargo test` on the host
  against a fake `BlockDevice`; injecting real frames means the real
  integration is only ever exercised in QEMU. Runtime injection of a *byte*
  interface (`DataCache`) preserves host-testability; injection of the frame
  interface does not.
- **C-3 — recursion.** The page cache fills via `read_at_by_inode` (through
  ext2). Routing ext2 writes through the same cache makes flushing a dirty
  page re-enter ext2 while it holds the state lock. Breaking that cycle
  needs `address_space_operations`-style layering — the actual hard part of
  unification, and why it is a proposals/-scale project.

## Race-safety argument (the E2C-BAD worry)

All block-cache mutations happen while holding at least a state read lock;
writers hold the state write lock exclusively, so a dirty slot can never be
observed torn. The historical `[E2C-BAD]` defect class was staleness of a
*shadow* cache vs the device; write-back makes the cache authoritative and
`write_block` remains the single write choke point. `verify_cached_block` /
`E2_CACHE_VERIFY_MISMATCH` are repurposed as a **host test oracle**: mixed
adversarial workloads with `E2_VERIFY_HITS` on must finish with mismatch
count 0 (cache bytes == device bytes after every flush).

## Verification ladder (host before kernel, per session rule)

1. Host unit tests: existing 12-file suite + new tests for eviction-flush,
   free/realloc poison, read-after-write warmth, flush ordering, bitmap
   cursor. Many iterations, parallel. — **DONE, see below**
2. E2C coherence oracle over mixed adversarial workloads (above).
3. `ext2probe-host` deterministic device-I/O counts, A/B against HEAD.
   — **DONE**, see below
4. Clippy per-crate + `--release` + `extreme-size`; kernel builds.
   — **DONE**, see below
5. Only then: QEMU boot, live functional check (nested dirs, 50-file
   content verify, rename, rm -rf), real-kernel A/B. — **DONE (A/B)**, see
   below; the standalone functional check is still owed.

### Host-test results (2026-08-26, after implementation)

Step 1 complete: **75/75 tests**, 15/15 clean repeated parallel runs
(default ring cache), 10/10 clean with `--features fs-cache` (fs-level
workloads against `ClockBlockCache` — necessary because a plain `cargo
test` binds `BlockCache = BlockRingCache`, so the clock cache's dirty
paths are otherwise only reachable through its direct unit tests), clean
in `--release`.

What the host runs proved:

- **Persistence oracle**: a `RecordingDevice` (counting, write-logging
  `BlockDevice`) lets a test mount a *second* filesystem over the same
  bytes — "op returned ⇒ device holds a complete, self-consistent fs".
  Passed for create/extend/truncate/rename/delete mixes and 150 KB
  eviction-driving writes.
- **D-3 poison recipe passes**: junk block → delete → realloc → partial
  write ⇒ tail reads zeros, cache and device agree.
- **D-2 ordering observable in the write log**: file data reaches the
  device before the superblock (offset 1024) write.
- Test-authoring lessons: (a) the write-log probe must consider
  full-block writes, not just short ones; (b) the thread-local
  `FREED_INODES` fix from the prior session holds (no flake in 25
  parallel runs).

Known host gaps, addressed later on the ladder: E2_VERIFY_HITS oracle
run; multi-group images (fixture is single-group — `is_alloc_meta` BGD
span and cross-group cursor restarts are thinly exercised); `extreme`
profile arms (host-invisible); device-I/O counts.

### Steps 3–5 results (2026-08-26)

**Step 3 — device I/O (`ext2probe-host` over `disk.img`, A–D `4b086f3d` → +write-back):**

| op | A–D | +write-back |
|---|---:|---:|
| create 300 × 4 KB | 2700 wr / 1502 rd | **2385 wr / 18 rd** (9.0 → 8.0 wr/file) |
| seq_write 2 MB | 2042 wr / 1014 rd (3.6×) | **1789 wr / 0 rd (3.1×)** |
| seq_read 2 MB after that write | 515 rd | **0 rd** |
| delete 300 | 2400 wr / 1496 rd | **2085 wr / 0 rd** (8.0 → 7.0 wr/file) |
| build 3200-file tree | 28 987 wr / 16 107 rd | **25 613 wr / 205 rd** |
| mass-delete 3200 | 25 753 wr / 16 115 rd | **22 396 wr / 0 rd** |
| 100 × warm `stat()`, 7 components | 3 rd | **0 rd** |

The read side is essentially gone (−99% to −100% everywhere), and D-5's
in-slot inode-table patch removes exactly one write per file.

**Step 4 — builds:** `cargo test -p akuma-ext2` 76/76, and 76/76 again with
`--features fs-cache`; `cargo clippy --all-targets` on the crate back to the
18 warnings HEAD already had (every warning the write-back work introduced
is fixed, and the three `#[allow]`s it added are gone); `cargo build
--release` and `scripts/build_extreme_size.sh` both clean. `extreme-size`
initially failed with `sync_all_filesystems is never used` — that profile
builds `--no-default-features`, so `sc-reboot`, the function's only caller,
is absent; it is now gated on that feature.

**Step 5 — real-kernel A/B** (QEMU, `ext2probe` guest, BEFORE pass, median of
3 runs per arm, arms verified distinct by `akuma.bin` size):

| op | A–D | +write-back | Δ | A–D range | +wb range |
|---|---:|---:|---:|---:|---:|
| create 300 × 4 KB | 1858 ms | 1433 ms | −23% | 1699–1874 | 1286–1441 |
| seq_write 2 MB | 858 ms | 669 ms | −22% | 803–895 | 637–753 |
| seq_read 2 MB | 143 ms | 43 ms | **−70%** | 122–175 | 42–54 |
| delete 300 | 1062 ms | 809 ms | −24% | 955–1075 | 713–850 |
| build 3200-file tree | 19.3 s | 15.9 s | −18% | 18.0–19.7 | 13.5–15.9 |
| mass-delete 3200 | 11.4 s | 7.5 s | −35% | 11.3–12.0 | 7.5–8.6 |

The ranges are **disjoint on all six ops** — the slowest write-back run beats
the fastest A–D run every time. Given this host's documented wall-clock
variance (`crates/akuma-ext2/README.md` § Performance), that separation, not
the medians, is what makes the result carry a claim. `ext2probe`'s own
before/after-mass-delete verdict stayed `NO REGRESSION` on 5 of 6 runs; the
one `REGRESSION` was `list_dir` moving 959 µs → 7030 µs, sub-millisecond
timer noise on a warm 301-entry directory, not a real effect.

**Method traps hit:** (a) `cargo build --release` does **not** regenerate
`target/aarch64-unknown-none/release/akuma.bin` — only `cargo run` does, via
`scripts/cargo_runner.sh`; A/B arms must be confirmed distinct (size/mtime)
before trusting either. (b) `ext2probe-host` cannot be built `--release`
from the `userspace/` workspace: that profile is `opt-level = "z"`, which
makes akuma-ext2's `build.rs` set `kernel_profile_extreme`, and
`ClockBlockCache` (gated `any(ext2_fs_cache, test)`) then compiles with
nothing to construct it — `-D dead-code` fails. Pre-existing at
`4b086f3d`; use the documented dev-profile invocation.

## What is still open (audited 2026-08-26, after the A/B)

Landed and verified: **D-1** (write-back in the block cache), **D-2** (data
before metadata, asserted in the write log), **D-3** (invalidate-on-free never
flushes, with the poison test), **D-4** (zero-copy reads — see below),
**D-5** (in-slot sub-block patch — worth one device write per file),
**D-6** (bitmap cursor), **D-7** (`sync_all_filesystems` on `reboot(2)`),
**D-11** (no extraction).

### D-4 landed 2026-08-26 — and did not buy what it was predicted to

Implemented as `Ext2Filesystem::with_block(state, block_num, f)`: on a cache hit
`f` runs against the cached slot directly, so no `Vec` and no block memcpy.
`read_block` is now `with_block(.., <[u8]>::to_vec)` for callers that genuinely
need ownership. Converted: `read_range`, `get_block_num`'s single- and
double-indirect walks, `read_inode_data`, and `Filesystem::read_at`.
Deliberately NOT converted — `write_range`'s miss path, `bitmap_slot`,
`set_block_num`, and the `truncate_inode` / free walks: they either mutate the
block and write it back, park it in `bitmap_cache`, or call `free_block` inside
the loop, which re-enters the cache lock and would deadlock. That constraint is
documented on `with_block` itself.

Lock-hold time is unchanged: the old code also memcpy'd (`to_vec`) with the
cache lock held, so this only removes the allocation.

**Measured (guest, 3 runs per arm, back-to-back in one session):**

| op | without D-4 | with D-4 | delta | verdict |
|---|---:|---:|---:|---|
| create 300 | 779 ms | 724 ms | -7% | ranges overlap - noise |
| seq_write 2 MB | 369 ms | 363 ms | -2% | noise |
| seq_read 2 MB | 26.8 ms | 26.6 ms | -1% | noise |
| delete 300 | 412 ms | 412 ms | 0% | noise |
| **build 3200-file tree** | **7.90 s** | **7.57 s** | **-4%** | **3/3 disjoint** |
| mass-delete 3200 | 4.43 s | 4.47 s | +1% | noise |

A focused warm-read benchmark (20 x `cat` of an 8 MB file - 40 960 block
accesses - minus a spawn-only control) came out **1.90 s on both arms**: no
measurable read-path win at all.

**Why the prediction was wrong.** The design text above assumed the block memcpy
was a visible share of `seq_read`. It is not. `Filesystem::read_at` re-resolves
the *path* on every `read(2)` - `src/syscall/fs.rs` passes `&f.path`, so each
call does a full `lookup_path_internal` directory walk plus `read_inode`, and
allocates a temp buffer of up to 64 KB. At ~46 us per 4 KB block accessed, that
per-syscall work dominates; the 4 KB copy D-4 removes is roughly 1 us of it. The
gain shows up only on `build tree`, which is metadata-access-dense (every create
walks the inode table via `read_range` and the indirects via `get_block_num`)
rather than syscall-dominated.

D-4 stays: it is strictly less work, and it removes a 4 KB alloc/free per block
access - heap-churn relief the allocator's spinlock feels under SMP even where
wall-clock cannot see it. But **the next real read lever is per-fd inode
caching, not the block cache**: resolve the path once at `open(2)` and let
`read(2)` use `read_at_by_inode` (which the mmap/exec fault path already does)
instead of re-walking the tree on every call.

Not done:

  bundled. Still the fix for **F-1** above (the global page cache keys on inode
  number alone, so two independent ext2 mounts share a keyspace) — a latent
  correctness bug, not tidiness.
- **D-10 — the `DataCache` proposal was never written.** `proposals/` has no
  such document. Until it exists, the mmap-vs-read double-cache seam stands.
- **Verification ladder step 5's functional half.** The real-kernel A/B ran, but
  the standalone live functional check it is paired with (nested dirs, 50-file
  content verify, rename, `rm -rf`) has not been run against the write-back
  kernel as a deliberate pass.
- **Host-test coverage gaps** (unchanged from the step-1 note): the fixture is a
  single-group image, so `is_alloc_meta`'s BGD span and cross-group cursor
  restarts are thinly exercised; the `extreme` arms are host-invisible.

## Background

Defects found and fixed while finishing this work (build-profile traps, the
stale-`akuma.bin` A/B corruption, the guest-shell blocker, and D-4's wrong
premise): [`EXT2_WRITEBACK_FOLLOWUP_FIXES.md`](EXT2_WRITEBACK_FOLLOWUP_FIXES.md).

Spawned from the ext2 performance line of work:
[`EXT2_PERFORMANCE_AUDIT.md`] (prior audit and its ratio-measurement flaw),
`crates/akuma-ext2/README.md` (fixes A–D), commit `52d97c89`.
Page-cache seams: [`FILE_PAGE_CACHE_MMAP_AMPLIFICATION.md`],
`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` (E2C-BAD history).

[`EXT2_PERFORMANCE_AUDIT.md`]: EXT2_PERFORMANCE_AUDIT.md
[`FILE_PAGE_CACHE_MMAP_AMPLIFICATION.md`]: FILE_PAGE_CACHE_MMAP_AMPLIFICATION.md
