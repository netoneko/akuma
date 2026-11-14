# A full pin-table window read "pinned", and ext2 never freed those unlinks

**Status: FIXED 2026-11-03** in `crates/akuma-primitives/src/inode_pin.rs` (`is_pinned`). The pin table is shared,
so the fix applies to the AArch64 kernel, amd64 and Taoxichuan alike.

## Symptom

On the Taoxichuan guest on the ryzen, a `-j3` cargo build was followed by `cargo clean` (2,923 files, 1.6 GiB). With
the guest stopped, `e2fsck -fn` on its image reported:

- **65 inodes** still set in the inode bitmap with no inode behind them;
- **4,327 blocks** (17 MB) still set in the block bitmap and owned by no file.

`debugfs` showed what they had been:

- regular files of 16 to 954 bytes, plus one of 5 MB, created during the build;
- `Links: 0`, and a `dtime` from the `cargo clean` run.

These were cargo's fingerprint and dep-info files. Cargo reads them and never maps them, so nothing could have held
them pinned.

## Cause

ext2's `release_last_link` defers the free of an unlinked inode while `inode_pin::is_pinned` says a mapping still
holds it. It drops the name, stamps `dtime`, keeps the blocks, and queues the inode for `drain_deferred_frees`, which
asks `is_pinned` again.

`is_pinned` walks the inode's 32-slot probe window (`PROBE_LIMIT`). It answers:

- `false` at the first empty slot;
- the count, if it finds the key;
- **`true` if it walks the whole window and finds neither.** The comment read: "the table is congested and absence
  cannot be proven".

Absence *was* proven. `acquire` puts a key only inside that same window: on a tombstone or an empty slot it passes.
When the window has neither, it counts the pin in `OVERFLOW` for the inode's region, and `is_pinned` checks that first.
So a full window without the key means the inode is not pinned.

A full window is easy to make. `release` leaves a tombstone, and `compact_tail` clears one only when the *next* slot is
empty. A run of tombstones followed by a live entry therefore stays. The hash (`inode * 0x9E3779B9`, bits 16..25)
puts inodes 9, 74 and 83 apart in neighbouring slots, so one build's artifacts fill a stretch of slots. All 65
stranded inodes are homed in pin-table regions 14 to 16, 48 of them in region 15. Every inode homed there read as
pinned for as long as the run lasted, so every unlink of one was deferred, and the drain skipped each of them.

This is the second half of the defect in [`EXT2_UNLINK_INODE_BLOCK_LEAK.md`](EXT2_UNLINK_INODE_BLOCK_LEAK.md) §2.6.
Compaction (§2.6a) fixed windows whose run ends in an empty slot. It could not fix a run held open by a live entry,
and the fall-through `true` was left in place.

## Fix

`is_pinned` returns `false` when the window is full and holds no entry for the key, with the reasoning above in place
of the old comment. The overflow check before it is unchanged, so a pin that `acquire` could not record still makes
its region read pinned.

A race between a concurrent `acquire` and `is_pinned` is no wider than before. A pin claimed behind the scan is the
same race as one claimed in an empty slot the scan already passed, and that case already answered `false`.

## Verification

- New host test `a_full_window_without_the_key_is_not_pinned` (`akuma-primitives`):
  - it fills one window exactly with colliding inodes, then drops all but the last, which leaves 31 tombstones
    `compact_tail` must keep;
  - it asserts that an absent collider and a tombstoned one read unpinned, and that the live one reads pinned;
  - it asserts that the last release compacts the run.

  It **fails with the old `true`** and passes with the fix. All 11 `inode_pin` tests pass.
- Guest image: `e2fsck -fy` returned exactly the 65 inodes and 4,327 blocks (26,046 → 25,981 files; 641,094 →
  636,767 blocks), and a recheck is clean.
- Field check, on the Taoxichuan guest running the fixed kernel (`kernel.pin`):
  - a clean `-j3` `cargo build -p infinity-daemon`: 180 crates, `rc=0`, 550 s;
  - then `cargo clean`, which removed 1,895 files (1.2 GiB);
  - free blocks came back to 410,604, against 410,605 before the build (the difference is the build log);
  - with the guest stopped, `e2fsck -fn` on the image is clean (`rc=0`, no bitmap differences).

## Background

- [`EXT2_UNLINK_INODE_BLOCK_LEAK.md`](EXT2_UNLINK_INODE_BLOCK_LEAK.md): the deferral, the pin table, and the
  tombstone fixes this completes.
- [`TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md`](TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md): the run that found
  it.
