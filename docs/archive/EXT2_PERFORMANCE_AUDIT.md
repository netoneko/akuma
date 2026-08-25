# ext2 performance after a bulk delete — audit

**Status: theories written up, benchmarked, no regression reproduced.**
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

## Background

[`GETDENTS64_DIR_CACHE_FIX.md`](GETDENTS64_DIR_CACHE_FIX.md) (the
256-subdirectory tree size this benchmark's default is modeled on),
[[project_isolated_qemu_verification]] (isolated-VM verification recipe this
followed).
