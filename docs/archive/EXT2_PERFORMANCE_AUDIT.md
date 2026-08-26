# ext2 performance after a bulk delete — audit

**Status: RESOLVED 2026-08-26 by a host I/O-probe follow-up — the cause is
unconditional write amplification, not a post-delete regression. See
"2026-08-26 follow-up" at the bottom, and
[`crates/akuma-ext2/README.md`](../../crates/akuma-ext2/README.md#performance-characteristics)
for the full write-up. First two fixes (deferred superblock/BGD metadata,
skipped redundant zero-fill) landed the same day: 2 MB write 5.7× → 4.1×
amplification, create 11.3 → 9.3 device writes/file. Directory-rewrite O(N²)
and the write-back block cache are still pending.**

Recorded 2026-08-25, triggered by a report of "fs performance looks like
shit" after running `rm -rf /tmp/akuma` on a live `devbox-smoltcp` guest.

The live guest's console (`-serial mon:stdio`, no log redirect) could not be
inspected after the fact, so this audit could not confirm what the user's VM
actually did. What follows is (1) a code-reading pass over the ext2/allocator
paths a mass delete touches, producing candidate theories, then (2) an
isolated-VM benchmark (`userspace/ext2probe`, new in this audit) built to test
those theories directly. The benchmark did not reproduce a regression at the
scale tested — see "Benchmark results" below — which weakens the theories
more than it confirms them.

## Theories (from reading the allocator/cache code)

Ranked as they were before benchmarking; kept in the original order for the
record even though (2) came back non-reproducing.

1. **Deferred-inode-free overflow leaking space.**
   `DeferredFrees` (`crates/akuma-ext2/src/ext2.rs:375-411`) holds unlinked-
   but-still-mapped inodes in a **fixed 256-slot** array; the actual block/
   bitmap free happens later, in `drain_deferred_frees`, once the last
   mapping pin drops. Push a 257th concurrently-pinned unlink and the
   overflow one is **permanently leaked** — `free_blocks_count`/
   `free_inodes_count` never get credited back. This only applies if the
   deleted files were concurrently mmap'd/pinned by something — a plain
   `rm -rf` of files nothing else has open does not hit this path; ordinary
   `rm` doesn't mmap what it deletes.
   - **Diagnosability gap found while checking this**: the counters that
     would prove or disprove it — `defer=`/`defer_leak=`/`pin=`/`pin_ovf=` in
     `dp_counters_line` (`src/pmm.rs:365-393`) — are **only ever printed from
     the sync-EL1 crash handler** (`src/exceptions.rs:3579`), not from the
     periodic 10s `[Mem]` line in `src/main.rs`. There is currently no way to
     read `defer_leak` from a healthy, non-crashing kernel. Worth exposing
     (a debug syscall, or folding into `[Mem]`) since it's exactly the
     counter this whole class of bug needs.

2. **Stale file-page-cache entries never invalidated.** `invalidate_inode`
   (`src/file_page_cache.rs:367`) is wired to the inode-*freed* hook
   (`src/fs.rs:57`), not to unlink. If (1) is happening, the cache entries for
   those files are pinned right along with the leaked inode, shrinking the
   effective cache for everything else. Contingent on (1); not independently
   evidenced.

3. **Bitmap read-modify-write churn during the delete itself.**
   `free_block`/`free_inode` (`ext2.rs:1325-1470`) do a read-modify-write of
   the relevant bitmap block per freed block/inode. There's a 64-entry block
   cache (`ClockBlockCache`/`BlockRingCache`, `ext2.rs:55-105`), so this is
   normally cheap, but a tree spread across many block groups could thrash it
   during the delete. Time-bounded to the delete itself, not a lasting
   regression — weakest fit for "still slow after the command returned."

4. **Free-space fragmentation** — the generic "real filesystem" explanation:
   deleting a big tree scatters free blocks, so later large writes get
   less-contiguous allocations. Not ext2/Akuma-specific, couldn't be ruled
   out from reading code alone.

## Benchmark: `userspace/ext2probe`

Added as a new `userspace/` member (`ext2probe/Cargo.toml`,
`ext2probe/src/main.rs`) to test the theories directly rather than by more
code reading. Follows the `fpcprobe`/`shareprobe`/`allocstress` convention:
a `#![no_std]` `libakuma` binary, workspace member, built via
`userspace/build.sh --ext2probe-only`.

**What it does** (see the file's module doc for full detail): a `BEFORE` pass
creates 300 4 KB files, does a 2 MB sequential write + read, lists the
directory, times all four, then deletes everything and reports elapsed time
per op. Then a **stress pass** builds a `dirs`-subdirectory ×
`files-per-dir`-file tree (default 16×200 = 3200 files, sized off the
256-subdirectory `go-build`-cache shape in
[`GETDENTS64_DIR_CACHE_FIX.md`](GETDENTS64_DIR_CACHE_FIX.md)) and mass-deletes
it in one timed pass — the actual `rm -rf`-shaped operation under test. Then
an identical `AFTER` pass repeats the `BEFORE` workload in a fresh directory.
The tool prints a before/after %-delta per op and a `REGRESSION`/
`NO REGRESSION` verdict line (>=20% slower on any op counts as a
regression), for a log grep. Usage: `ext2probe [stress_files_per_dir]
[stress_dirs]`.

**Verification method**: booted a fully isolated instance so as not to
disturb the live self-hosting VM — `cp -c` clone of `devbox.img`, `e2fsck -fy`
via a privileged Docker loop mount (also used to inject the freshly built
`ext2probe` binary into `/bin`), current `target/.../akuma` ELF copied
(not rebuilt) into scratch, launched directly via `scripts/cargo_runner.sh`
with `INSTANCE=3` (ports +300) so it never touched `devbox.img` or
`target/aarch64-unknown-none/release/akuma.bin` — the latter is live
self-hosting's `KERNEL_DROPOFF` target on the real VM and must not be
regenerated out from under it. See
[[project_isolated_qemu_verification]] for the recipe this followed.

### Results

Default size (16×200 = 3200 files, ~12.5 MB):

| op | before | after | delta |
|---|---|---|---|
| create (300×4KB) | 430445 us | 279430 us | **-35%** |
| seq_write (2 MB) | 432333 us | 227095 us | **-47%** |
| seq_read (2 MB) | 20948 us | 22250 us | +6% |
| list_dir (300 entries) | 1264 us | 1380 us | +9% |

Verdict: `NO REGRESSION`.

Scaled up (40×500 = 20,000 files, ~78 MB) to check whether the effect
reverses at larger scale — mass delete alone took ~16.9 s (1184 files/sec):

| op | before | after | delta |
|---|---|---|---|
| create | 380147 us | 285160 us | **-24%** |
| seq_write | 290014 us | 237010 us | **-18%** |
| seq_read | 20585 us | 22734 us | +10% |
| list_dir | 2234 us | 944 us | **-57%** |

Verdict: `NO REGRESSION`, same direction as the smaller run.

**Reading this result**: ops got *faster* after the mass delete, not slower,
at both scales tested. The most likely explanation is a confound in the A/B
design rather than a genuine ext2-level speedup: the `BEFORE` pass runs right
after boot with cold `ClockBlockCache`/`BlockRingCache` entries (superblock,
group descriptors, bitmap blocks), while the stress pass that runs in between
touches every block group and warms exactly that metadata, so the `AFTER`
pass benefits from a hot cache independent of anything the delete itself did
to the filesystem's on-disk state. This benchmark cannot separate "cache
warmth" from "real structural effect" within a single boot — that would need
a design that reboots (or otherwise cold-starts the cache) between the
`BEFORE` and `AFTER` passes while preserving the post-delete on-disk state.

## Conclusion

At the scale tested (up to 20,000 files / ~78 MB, single boot session), a
bulk create+delete does **not** reproduce a measurable ext2 operation
regression — if anything the opposite, plausibly a cache-warmth artifact of
the benchmark's own design rather than a real effect. This means:

- Theories 3 and 4 (bitmap churn, fragmentation) are not supported by this
  benchmark at this scale. They aren't ruled out at a scale/fragmentation
  pattern this synthetic benchmark didn't reach.
- Theories 1 and 2 (deferred-free leak, stale fpcache pin) remain untested —
  they require files still mapped elsewhere at unlink time, which
  `ext2probe`'s plain `open`/`write`/`close`/`unlink` cycle never produces.
  Testing them needs a probe that unlinks a file while it (or another
  process) holds it `mmap`'d, which `libakuma` doesn't currently wrap for
  file-backed (non-anonymous) mappings — the `mmap()` convenience wrapper in
  `userspace/libakuma/src/lib.rs` only has `MAP_PRIVATE|MAP_ANONYMOUS`
  constants exposed today.
- The original report's `/tmp/akuma` was a real self-hosting checkout on the
  live VM (source tree + `.git` + build artifacts), a different shape (much
  deeper nesting, larger files, likely some files open by a running build)
  than this synthetic flat tree — this audit did not have access to that
  VM's disk without disturbing the running self-host loop, so it could not
  be replayed byte-for-byte.
- It's also plausible the original slowness had nothing to do with ext2 at
  all (general host/QEMU load, something else running concurrently) and
  coincided with the `rm -rf` rather than being caused by it.

## Follow-ups if this is revisited

1. Expose `defer`/`defer_leak`/`pin`/`pin_ovf` outside the crash handler so
   theory 1 can be checked on a live, non-crashing kernel.
2. Extend `ext2probe` (or a sibling probe) to unlink-while-mmap'd, to
   actually exercise the deferred-free path — needs a `libakuma` file-backed
   `mmap` wrapper first.
3. Redesign the A/B to eliminate the cache-warmth confound: cold-boot between
   `BEFORE` and `AFTER` (persist the disk across a `SNAPSHOT=0` reboot rather
   than doing both passes in one boot).
4. If the user can characterize `/tmp/akuma`'s actual shape (roughly how many
   files/dirs, nesting depth, whether a build was live against it), size
   `ext2probe`'s stress phase to match rather than guessing from the
   `go-build`-cache precedent.

## 2026-08-26 follow-up — the actual cause (host I/O probe)

The 2026-08-25 benchmark asked the wrong question. It measured op timings
*before vs after* a bulk delete and looked for a delta. There is no delta —
the cost is a **constant multiplier that applies to every write, always**, so a
before/after ratio is ~1.0 (which is exactly what it saw, and the "faster after"
was cache warmth as the audit already suspected). Fragmentation (theories 3, 4)
and the deferred-free leak (theories 1, 2) are not involved.

### Method

Not a devbox boot. `Ext2Filesystem` (the `crates/akuma-ext2` crate, host-linked
against `std`) mounted over an in-RAM copy of a real 256 MB `mke2fs` image
(`-b 4096`, 2 block groups), with a `BlockDevice` shim that counts every
`read_bytes` / `write_bytes` call and buckets it by on-disk region (superblock /
GDT / bitmap / inode-table / data). Call count is the right metric: each
`write_bytes` is one synchronous, busy-polled virtqueue round-trip on the guest
(`crates/akuma-virtio/src/block.rs`), and it is a *sector* read-modify-write, so
a 1 KB superblock write is a 2-sector read + 2-sector write.

### Findings

| operation | device **write calls** | write bytes | note |
|---|---:|---:|---|
| create 1 × 4 KB file | **11.3** | ~39 KB | sb×2, gdt×2, bitmap×2, inode-table×2, data×3.3 |
| delete 1 × 4 KB file | **10.0** | ~34 KB | sb×2, gdt×2, bitmap×2, inode-table×2, data×2 |
| 2 MB sequential write | **3327** | **11.7 MB → 5.7× amplification** | 514 superblock writes alone |
| 2 MB read back (warm) | 0 | 0 | block cache absorbs it |
| build 3200-file tree | 11.1 / file | linear | |
| mass-delete 3200-file tree | 9.1 / file | linear | |
| Nth file into one flat dir | **11 → 18** as N: 0 → 2000 | grows | dir rewrite is O(N) → fill/empty is O(N²) |

Ranked causes (full detail in the crate README):

1. **No write-back / write coalescing at any layer.** Every `write_block`,
   `write_inode`, `write_bgd`, `write_superblock` goes straight to the device.
   The block cache is *invalidated* on write, never updated. `sync()` is a no-op.
2. **Full 1 KB superblock rewrite on every block and every inode alloc/free** —
   only to adjust `unallocated_blocks` / `unallocated_inodes`.
3. **Full BGD rewrite the same way** (`free_*_count`).
4. **`allocate_block` zero-fills each new block with its own device write**, then
   the caller overwrites it immediately — one wasted full-block write per block.
5. **`add_dir_entry` / `remove_dir_entry` rewrite the entire directory** on every
   entry → O(N²) to fill or empty one directory.
6. `allocate_block` linear-scans the block bitmap from bit 0 every call.

"Slow after `rm -rf /tmp/akuma`" was the `rm -rf` *itself* (files × ~9 synchronous
device writes = tens to hundreds of thousands of virtqueue round-trips), plus
every subsequent build `create` costing ~11 writes. Nothing about the delete
degraded the filesystem's later state.

The harness also confirmed a separate concern: the crate allocates freely — and
in `read_inode_data`'s case, *unboundedly* (whole directory into a `Vec` on every
path-lookup component) — while the `no-bkl-vfs` `PreemptGuard` holds local IRQs
masked. See the README's "Allocation audit".

### Fixes applied 2026-08-26 (checkpoint 1)

Two low-risk changes in `crates/akuma-ext2/src/ext2.rs`, host-tested (63/63) and
kernel-build-clean:

- **Fix A — skip redundant zero-fill.** `ensure_block` takes a `zero_leaf` flag;
  `write_inode_data` and the full-block arm of `write_at` pass `false` because
  they write every byte of the block immediately after. Removes one full-block
  device write per allocated data block (512 on a 2 MB write). Indirect pointer
  blocks and partially-written blocks are still zeroed.
- **Fix B — defer superblock + BGD writes.** `Ext2State` gains an in-memory
  `bgd_cache` + `bgd_dirty` + `sb_dirty`; the four allocators stage updates in
  memory and `flush_meta` writes the dirty ones once at the end of every mutating
  `Filesystem` method and on `sync()` (no longer a no-op). On-disk metadata stays
  consistent at every syscall boundary.

Probe deltas (`ext2probe-host`, `disk.img`): create 11.3 → 9.3 wr/file, delete
10.0 → 9.0, 2 MB write 3327 → 2300 calls / 5.7× → 4.1×, tree build −18%,
mass-delete −11%. Pending: incremental directory writes (the O(N²)), write-back
block cache (read-after-write is still cold), bitmap-scan cursor.

## Background

[`GETDENTS64_DIR_CACHE_FIX.md`](GETDENTS64_DIR_CACHE_FIX.md) (the
256-subdirectory tree size this benchmark's default is modeled on),
[[project_isolated_qemu_verification]] (isolated-VM verification recipe this
followed), [`crates/akuma-ext2/README.md`](../../crates/akuma-ext2/README.md)
(the 2026-08-26 follow-up's home, with diagrams and the allocation audit).
