# ext2 dirent types: written where the format forbids them, read as "file" where unknown

**Status: FIXED 2026-11-02** in `crates/akuma-ext2`, host-tested. Found on a Taoxichuan guest disk during
[`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md). The guest kernel that carries the fix has not been
rebuilt yet; the disk that showed the bug was repaired instead (below).

## Symptom

A guest disk (`/root/tx/rs.goose.orig`, copied and grown, ext2, `Filesystem features: (none)`) was stopped
and fsck'd on the host. e2fsck spent four minutes on:

```
Entry '.vim' in /root/.cargo/registry/src/.../aho-corasick-1.1.5 (27634) has filetype set.
Clear? yes
```

42,815 times. After the guest booted again, git in `/src/infinity` showed every top-level directory as
untracked (`?? crates`, `?? docs`, ...) although the index and `HEAD` both listed the same 422 files, and
`git fsck` said:

```
error: refs/heads: invalid sha1 pointer 0000000000000000000000000000000000000000
```

git was reading the directory `.git/refs/heads` as if it were a ref file. Python's `os.scandir` showed
the same thing: `d_type` said "not a directory" and `stat` said directory, for all three of `refs/`'s children.

## Cause: two halves of one rule

ext2 directory entries have a type byte (byte 7) only on a filesystem with `INCOMPAT_FILETYPE` (`0x2`).
Without it, byte 7 is the high half of a 16-bit `name_len`, which must be 0.

1. **Write side.** `add_dir_entry` and `create_dir`'s `.`/`..` stored `FT_REG_FILE`/`FT_DIR`/... in byte 7
   on every filesystem. On one without the feature that is a malformed `name_len`, which is why e2fsck
   cleared all 42,815.
2. **Read side.** `read_dir` built `DirEntry { is_dir, is_symlink }` from byte 7 alone. `DirEntry` has no
   "unknown", so byte 7 = 0 meant "regular file". After e2fsck zeroed the bytes, every directory the guest
   had created listed as a file, and `getdents64` reported `DT_REG` for it. The image's original entries
   (made by `debugfs` on a `(none)` filesystem) had 0 all along; the bug was invisible only because the
   tools that walked them (`ls -l`, shells) `stat` anyway.

## Fix

- `stored_dirent_type(state, ft)`: the byte written is `ft` when the superblock declares
  `INCOMPAT_FILETYPE`, else 0. It is used by `add_dir_entry` (every caller) and by `create_dir`'s `.`/`..`.
- `read_dir`: the byte is trusted only on a filesystem with the feature, and only when nonzero. Otherwise
  the inode decides (`type_perms & 0xF000`). Every non-directory's inode was already read for its size, so
  this adds no read for files.
- `read_inode_data` and `parse_directory` became `pub(crate)` for the tests, the crate's existing convention.

## Repair of the affected disk

The guest kernel was not rebuilt mid-run. The disk was repaired on the host instead:
`tune2fs -O filetype disk.img; e2fsck -fy disk.img`. That turned the feature on (it is what the guest's
writes had assumed) and e2fsck set the type of 60,042 entries from their inodes. git was then clean and
`d_type` matched `stat`.

## Verification

- `without_the_filetype_feature_dirents_store_no_type_and_read_dir_asks_the_inode`: the `test.ext2`
  fixture with `INCOMPAT_FILETYPE` cleared. New directories list as directories, and every entry the test
  creates (including `.`/`..`) stores 0. With the read fix reverted it fails on "a directory with no stored
  type is still a directory" (`left: Some(false)`).
- `with_the_filetype_feature_dirents_store_their_type`: the fixture as built stores 2 for a directory and 1
  for a file, so `getdents64` keeps reporting `d_type` without an inode read.
- `cargo test -p akuma-ext2`: 137 passed.

## Still open

- The Taoxichuan guest kernel and the Akuma kernels pick the fix up on their next build. Until then, keep
  guest images on a filesystem with `filetype` (`mke2fs -t ext2` sets it by default; `/root/tx/mkimg.sh` on
  the ryzen produced `(none)`).
- `akuma-ext2` has no triple-indirect blocks: a file stops at 12 + 1024 + 1024² blocks (4,299,210,752 bytes
  with 4 KiB blocks). That caps a guest disk image on an Akuma host at about 4 GiB.
