# ext2: no file could pass about 4 GiB

**Status: FIXED 2026-11-03** in `crates/akuma-ext2/src/ext2.rs`. **Not yet deployed** to the ryzen Akuma host, whose
root ext2 holds the Taoxichuan guest image: that needs a new host kernel and a reboot, planned for the Pop KVM session.

## Symptom

Growing the Taoxichuan guest image on the ryzen host to 16 GiB failed: `truncate` reported `I/O error`. The image
stayed at 4,289,998,848 bytes, and a clean `-j3` build of the whole Infinity workspace in the guest ran out of disk at
crate 264 of 278 ([`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md), "The disk is capped").

## Causes

Two caps, each about 4 GiB:

1. **No triple-indirect blocks.** `get_block_num` and `ensure_block` handled direct, singly-indirect and
   doubly-indirect pointers and ended in `Err(FsError::NotSupported)`. With 4 KiB blocks that is 12 + 1024 + 1024²
   blocks, 4,299,210,752 bytes; with 1 KiB blocks, about 64 MiB. `free_blocks_from` and `truncate_inode` never looked
   at `i_block[14]`.
2. **Only `i_size_lo` was read.** `truncate` and `fallocate` kept `i_size_high`, but `metadata`, `read_at`,
   `read_at_by_inode`, `read_dir` and `write_at` used `size_lower` alone. Past 4 GiB a file's size would have read
   modulo 2³², and `write_at` would have extended only the low word.

## Fix

- `Inode::file_size()` / `set_file_size()`. A regular file's size is `i_size_lo | i_size_high << 32`. Anything else
  uses the low word only, because the field is `i_dir_acl` there, as in Linux's ext2. Every read and write of a
  regular file's size goes through them, including `write_inode_data` and `truncate_inode`, which now clear the high
  word too.
- Triple indirection in `get_block_num` (three pointer reads, `None` at the first zero) and `ensure_block` (each
  pointer block is allocated zeroed the first time a block under it is). A recursive `free_tree_from(block, level,
  first)` frees everything at or past a logical index under one pointer block. Truncate, unlink and `O_TRUNC` use it
  for the triple-indirect tree. The existing single- and double-indirect code is unchanged.
- `note_large_file`: the first regular file past 2 GiB sets `RO_COMPAT_LARGE_FILE` in a revision-1 superblock, as
  Linux does. Otherwise `e2fsck` and other readers are told nothing reads `i_size_high`.

## Verification

`a_file_past_4_gib_uses_the_triple_indirect_map_and_its_full_size` (`crates/akuma-ext2/src/tests.rs`), on the 1 KiB-block
`manyinodes.ext2` fixture, where triple indirection starts at 64.3 MiB:

- sparse writes at 0, 70 MiB (triple-indirect) and 5 GiB + 1020 (past 4 GiB, straddling a block boundary);
- size and bytes read back through the same mount and through a fresh mount of the device;
- a hole at 3 GiB reads zero;
- `e2fsck -fn` is clean on the device image, after the writes and again after truncating;
- truncates into the triple-indirect range, out of it, and to zero, after which the free-block count is back at its
  baseline: every data and pointer block was returned.

All 138 `akuma-ext2` host tests pass.

## Still open

- **Deploy to the ryzen host** and grow `/root/infinity/disk.img` (then `resize2fs` it), at the Pop KVM session's
  reboot.
- **ENOSPC leaves the filesystem inconsistent.** After the whole-workspace build hit `No space left on device`,
  `e2fsck -fn` on the guest image found two problems:
  - an object file whose `i_size` (262,144) was short of its blocks (315,392);
  - an inode marked in the bitmap with no inode behind it, plus a group's directory count one too high: a `mkdir`
    that failed half-way.
  `write_inode_locked` returns on an allocation error without publishing the size of what it already wrote, and the
  directory create does not roll back its inode. Repaired offline with `e2fsck -fy` on 2026-11-03; the bugs are not
  fixed.

## Background

- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md): the run that found the cap.
- `docs/reference/subsystems/vfs.md`, "ext2": the rule this establishes.
