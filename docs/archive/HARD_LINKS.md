# Hard links

**Date:** 2026-10-14
**Status:** Implemented 2026-10-14 — see §4 for what landed and how it was verified.
**Part of:** the `box build` track — [`UNIX_PERMISSIONS.md`](UNIX_PERMISSIONS.md)
→ this → [`BOX_BUILD.md`](BOX_BUILD.md).

---

## 1. Current state (verified 2026-10-14)

`link(2)` is emulated by copying, and the emulation is wrong in three ways:

| Fact | Location |
|---|---|
| `sys_linkat` = `read_file(src)` + `write_file(dst)`: whole file into a kernel `Vec` | `akuma-syscalls-glue/src/fs.rs:3326` |
| **No `EEXIST`**: ext2 `write_file` truncates an existing file, so `link(a, b)` silently **overwrites** `b`. `link()`-as-exclusive-create (maildir, lock files) has two winners | `akuma-ext2/src/ext2.rs:3710-3720` |
| `olddirfd`/`newdirfd` used, `flags` ignored (`AT_SYMLINK_FOLLOW`, `AT_EMPTY_PATH`) | same |
| Linking a directory "works" or fails with an fs error, never `EPERM` | same |
| `st_nlink` is always the conventional 1 / 2: ext2's `metadata_of` returns `links: None` | `ext2.rs:3211` |
| `akuma-tar` turns every hard-link entry into a **relative symlink**, because `linkat` copied | `userspace/tar/src/extract.rs:251-293` |
| amd64: x86_64 `link` (86) has no `AT_FDCWD` shim → reaches nothing | `amd64/src/usermode.rs` (only `linkat` 265 routes) |
| No `Filesystem::link` in the VFS trait | `akuma-vfs/src/types.rs` |

What **already works**, which makes this small:

- ext2's inode has `hard_links`, read and written (`ext2.rs:864`).
- `remove_file` removes the dirent, decrements, and calls `release_last_link`
  **only at 0** (`ext2.rs:3886-3915`). `rename` over a destination does the same
  (`:4049`). Directories keep `2 + subdirs` (`:3837`, `:3876`, `:3952`).
- The file-page cache and deferred frees key on **inode**, so two names sharing
  one inode already share pages and are freed once.
- `add_dir_entry(state, dir, name, inode, file_type)` exists (`ext2.rs:2875`).

So the on-disk model has supported hard links all along; only the syscall and the
trait are missing.

---

## 2. Design

### 2.1 Trait

```rust
/// Add `new_path` as another name for the inode at `old_path`.
/// NotSupported maps to EPERM, which is what Linux returns from a filesystem
/// without hard links — and what `cp -l`, `tar` and cargo fall back on.
fn link(&self, old_path: &str, new_path: &str) -> Result<(), FsError> {
    Err(FsError::NotSupported)
}
```

### 2.2 ext2

Under one `write_state()` hold:

1. Resolve `old` → inode. Directory → `PermissionDenied` (→ `EPERM`).
2. `new` exists → `AlreadyExists` (`EEXIST`). Checked **inside** the hold: a
   check before it is the race that makes `link`-as-lock useless.
3. `hard_links == 65000` (`EXT2_LINK_MAX`) → `EMLINK` (new `FsError::TooManyLinks`).
4. `add_dir_entry(parent(new), name, inode, file_type_of(inode))`.
5. `hard_links += 1`, `ctime = now`, write inode, `flush_meta`.

Plus `metadata_of` returns `links: Some(hard_links)`, so `stat` tells the truth.

**Risk in that last line:** directory link counts become *visible*. GNU `find`'s
leaf optimisation skips descending when a directory's `nlink` says it has no
subdirectories; a directory whose count is understated (old damage —
[`AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`](AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md)) would then hide its children from `find`.
busybox `find` does not do this; GNU findutils in an image would. Run the ext2
audit tool over the devbox and metal disks before shipping, and have it check
`nlink == 2 + subdirs` for every directory.

### 2.3 Other filesystems

| fs | `link` |
|---|---|
| ext2 | §2.2 |
| overlay | copy up `old` (whole file, as for a write), then `upper.link(old, new)`; `new` must pass the same `prepare_create` as any create (clears a whiteout). Same semantics as Linux overlayfs without `index=on`: the link joins the upper copy, not the lower inode |
| `SubdirFs` | passthrough |
| memfs / tmpfs | `NotSupported` → `EPERM` first; a refcounted node later if something needs it |
| procfs / devfs | `NotSupported` |
| across mounts | `EXDEV`, decided in the glue before any fs is called |

### 2.4 Syscall

`sys_linkat` becomes: resolve both paths at their dirfds; `AT_SYMLINK_FOLLOW`
→ resolve `old` through symlinks, otherwise link the symlink itself (Linux's
default); `AT_EMPTY_PATH` → link the fd's own inode (rare; `EINVAL` until
someone needs it); different mounts → `EXDEV`; else `fs.link`. **Zero heap.**
Delete the copy path entirely — keeping it as a fallback brings the overwrite bug
back.

amd64: add `86 => linkat(AT_FDCWD, a1, AT_FDCWD, a2, 0)`. While there,
`chown` (92) and `lchown` (94) have no shims either, and the `fchown` (93) shim
passes the fd as a dirfd with a NULL path — harmless while `fchownat` returns 0,
wrong the day it does. Fix all three with UNIX_PERMISSIONS step 4.

### 2.5 Userspace follow-ups

- `akuma-tar` extract: real `link()`, falling back to the relative symlink only
  on `EPERM`/`EXDEV`. Re-pulled layers then match the registry's tree exactly.
- `akuma-tar` create (needed by `box build`): track `(dev, ino)` for every
  regular file with `nlink > 1` and emit later names as typeflag `'1'`.

---

## 3. Verification

- **Host (ext2):** link + read through both names; write via one, read via the
  other; unlink one, data survives; unlink both, block and inode bitmaps return
  to their pre-link counts; `rename` over a linked name; `EEXIST`, `EMLINK`,
  directory → `EPERM`; `nlink` reported.
- **Host (overlay):** link a lower file → copy-up, both names in upper, same
  inode; link onto a whited-out name.
- **Linux A/B probe** `userspace/linkprobe/` (static musl): errno vector for each
  case above + `st_ino` equality + `st_nlink`, plus two processes racing
  `link(tmp, lock)` 1000× — exactly one winner each round. Same binary on Akuma
  and Docker Linux; diff.
- **Guest:** `cp -al` a tree and `du -sh` both; re-pull an image with hard links
  (e.g. one shipping `git`) and compare its layer size before/after.

## 4. Implementation record (2026-10-14)

Landed as sketched, plus:

- **`FsError::CrossDevice` (`EXDEV`) and `FsError::TooManyLinks` (`EMLINK`)**,
  and `EXDEV`/`EMLINK` in `akuma_primitives::errno`. `vfs-glue` gained
  `resolve_same_mount`, shared by `rename` and `link`; a cross-mount *rename*
  used to answer `NotSupported` → `EIO`, which `mv` reports instead of falling
  back to copy-and-delete. It is `EXDEV` now.
- **ext2's `rename` wrote `FT_REG_FILE` for every non-directory**, so a renamed
  symlink or socket came back from `getdents64` as `DT_REG`. One
  `dirent_type()` helper now serves `rename` and `link`.
- **Directory link counts are reported too.** The worry in §2.2 cut the other
  way: the old constant `2` claimed every directory had *no* subdirectories,
  which is the value GNU `find`'s leaf optimisation acts on. The real count can
  only be more right; a directory whose on-disk count is damaged is the audit
  tool's to find.
- **`AT_EMPTY_PATH`** links the file the fd names (it was going to be `EINVAL`).
- **`fs.protected_hardlinks`** under `/proc/dac`: a non-owner may link a file
  only if it could read and write it (`EPERM`).
- `libakuma::link`; `akuma-tar` makes real links and keeps the relative symlink
  only as the fallback for `EPERM`/`EXDEV`.

### Verification

| Where | Result |
|---|---|
| Host | ext2 +6 (two names one inode; unlink keeps data and the last frees the blocks, measured in free-block counts; `EEXIST`/dir/missing; rename over a link; directory counts; renamed symlink keeps `DT_LNK`), overlay +1 |
| Linux (Docker, arm64) | `linkprobe` 23/23, including 200 rounds of two processes racing `link(tmp, lock)` |
| AArch64 QEMU/HVF | `linkprobe` 23/23 (0 double-winner rounds); `ln` + `du` count the data once; `akuma-tar` extracts a macOS-made archive's hard link as one inode with `st_nlink` 2 and the archive's uid; inside a container `ln` on the overlay works and a directory is `EPERM` |
| Boot suites | amd64 QEMU 807/0; AArch64 Firecracker 299/1 — the same pre-existing `thread_slot_reclaim_on_spawn` as unmodified HEAD |

**Not covered:** `lstat` of a *symlink* still reports `st_ino 1`, `st_nlink 1`
(`newfstatat`'s symlink arm builds the stat by hand), so a hard-linked symlink
reads as singly linked. Unchanged by this work.

## Background

- [`AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`](AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md) — the 2026-10-05
  lost-bitmap-writes incident; why link counts may already be wrong on old disks
- `userspace/tar/src/extract.rs:251` — the symlink workaround this retires
