# Deleting a file that had been mapped or open did not give its space back

**Status: FIXED 2026-11-03**, in two places:

- `crates/akuma-tokio-kernel-amd64/src/lazy.rs`: a Taoxichuan pin now covers a VA range;
- `crates/akuma-primitives/src/inode_pin.rs` and `crates/akuma-ext2/src/ext2.rs`: a deferred free now finishes once
  the last pin goes. This part applies to every kernel.

## Symptom

On the Taoxichuan guest on the ryzen, a 16 MB file was written, mapped, read, unmapped and closed, then unlinked. Its
4,101 blocks did not come back:

| after | 16 MB file's blocks |
|---|---|
| `unlink`, in the same process | in use |
| another file created and unlinked by that process | in use |
| the process exited | **in use** |
| an unrelated `touch x; rm x` | free |

Linux frees them at the `unlink`. Two related cases behaved the same way:

- **An open fd:** a file unlinked while an fd was open stayed allocated after `close(2)`.
- **A crash:** after a crash the stranded inodes are `e2fsck` bitmap differences, as
  [`INODE_PIN_CONGESTED_WINDOW.md`](INODE_PIN_CONGESTED_WINDOW.md) saw.

`userspace/forktest/c_stress/unlinkspace` passed 1 of 4 checks on the old kernel.

## Causes

1. **Taoxichuan pinned for the address space's lifetime.** `lazy::pin` kept one `InodePin` per `(root, inode)`, and
   only `free_user_root` dropped it, at reap. `munmap` never did, so ext2 saw the inode as mapped and deferred the
   unlink. The design said so ("coarser than Linux") in
   [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md).
2. **ext2 finished a deferred free only on the next unlink, rename or inode allocation.** Dropping the last pin is not
   a filesystem call: an `munmap`, a `close`, a reap. Nothing told ext2 the inode had become freeable, so the blocks
   stayed until an unrelated file was deleted or created. This is why the case above stayed in use after exit, and
   why the open-fd case stayed in use after `close`. That case reaches every kernel: an fd's `KernelFile` holds an
   `InodePin`.

## Fix

**Pins cover a range (Taoxichuan).** A pin entry is `(root, InodePin, lo, hi)`, the VA range that holds that inode's
records in that space:

- `pin` widens the range of an existing entry;
- fork copies it;
- `mremap`'s move widens it to where the records went (`move_pins`).

`release_pages` is the single path through which `munmap`, `MAP_FIXED`, `brk` and `mremap` give pages back. It now
trims every pin of the root by the released range, and then drops a pin when both of these hold
(`lazy::unpin_released`):

- its remaining range holds no unfilled record of the inode;
- no shared record there still writes back by inode.

A filled page holds its own frame and no longer needs the inode. The check walks only the pin's remaining range, and
gives up and keeps the pin past 65,536 pages. It allocates nothing.

**ext2 drains when a pin goes (shared).** The pin table counts last-pin releases (`inode_pin::unpin_epoch`). Every
ext2 write hold (`write_state`) compares that count with the value its mount last drained at, and drains the deferred
list when it has moved, flushing the free counts if anything was freed. `statfs` does the same, so `df` shows the
space as soon as the last mapping or fd goes, and `sync` lands it on disk.

## Verification

- Host tests:
  - new `statfs_after_the_last_pin_goes_shows_the_space_freed` (`akuma-ext2`): unlink a pinned 64 KB file, then drop
    the pin, and `statfs` alone returns every block. It **fails without the drain in `write_state`**;
  - all 139 `akuma-ext2` tests and all 64 `akuma-primitives` tests pass.
- Clippy is clean on Taoxichuan (with and without `disk`), amd64 and the AArch64 kernel.
- Ryzen guest, `unlinkspace /root`:

  | check | old kernel | new kernel |
  |---|---|---|
  | `after_munmap` | FAIL (4,101 blocks held) | PASS (0) |
  | `while_mapped` | PASS | PASS (still held, and still readable) |
  | `at_munmap` | FAIL (4,101) | PASS (0) |
  | `at_close` | FAIL (4,101) | PASS (0) |

  `shmshare` 8/8 and a `git init/add/commit` still pass.
- Ryzen guest, build and clean, with no extra drain step:
  - a clean `-j3` `cargo build -p infinity-daemon`: 180 crates, `rc=0`, 567 s;
  - `cargo clean` removed 1,895 files (1.2 GiB), and free blocks came back to 410,533, against 410,537 before (the
    difference is the logs);
  - with the guest stopped, `e2fsck -fn` is clean (`rc=0`, no bitmap differences).

## Background

- [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md): the per-root pin this replaces.
- [`EXT2_UNLINK_INODE_BLOCK_LEAK.md`](EXT2_UNLINK_INODE_BLOCK_LEAK.md) and
  [`INODE_PIN_CONGESTED_WINDOW.md`](INODE_PIN_CONGESTED_WINDOW.md): the deferral and the pin table.
