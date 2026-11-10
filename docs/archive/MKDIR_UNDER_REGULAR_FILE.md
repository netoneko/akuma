# `mkdir` (and every create) under a regular file succeeded and corrupted the file

Found 2026-11-10 while chasing the Bash-tool `EIO ... mkdir` in
[`CLAUDE_COMPATIBILITY_ISSUES.md`](CLAUDE_COMPATIBILITY_ISSUES.md) §1. **Status: FIXED** in
`akuma-ext2` (this is a separate, real bug; it is *not* shown to be the cause of that `EIO`).

## Symptom

On the trashcan (ext2 root, kernel `c523598a`), with Bun 1.4.3 and with busybox alike:

```sh
echo x > mt/f          # 2 bytes, nlink 1
mkdir mt/f/x           # rc=0   (Linux: ENOTDIR)
stat -c '%s %h' mt/f   # 4098 2 — grew by a block, nlink bumped
od -c mt/f             # "x\n" then a raw ext2 dirent for "x" at offset 2
touch mt/f/t           # rc=0   (Linux: ENOTDIR)
mkdir mt/f/y/z         # ENOTDIR (correct: here f is walked *through*)
rmdir mt/f/x           # ENOTDIR
```

`mkdir -p f/x` from Bun's `fs.mkdirSync(.., {recursive:true})` reports success too.

## Cause

`Ext2Filesystem::lookup_path_internal` checks that every component it walks **through** is a
directory, but never the last one. `lookup_parent_internal` resolves `parent_path` with it and
returned the inode as-is, so for `f/x` with `f` a regular file the "parent" was `f`. `create_dir`,
file create, `link`, `symlink`, `rename`, … then ran `add_dir_entry` on it, writing a dirent into the
file's data and bumping its link count. Any later reader sees a corrupt file; `rmdir`/`rm` of the
bogus entry fail with `ENOTDIR` because the lookup (correctly) refuses to descend into a file.

## Fix

`lookup_parent_internal` now reads the parent inode and returns `FsError::NotADirectory` unless it is
`S_IFDIR` — one check for every caller. Regression test:
`creating_under_a_regular_file_is_not_a_directory` (fails without the fix with `Ok(())`).

## Not shown / open

- This is **not** shown to explain Claude Code's `EIO: i/o error, mkdir '<scratchpad>/tasks'`. Bun's
  recursive mkdir over existing and fresh paths (0700, repeat, async) all behave on the trashcan; the
  `EIO` is not reproduced yet. It remains an open suspect only if a stale non-directory sits on that
  path, which `ls -ld` on the real path will say.
- Other filesystems (`memfs`, procfs, devfs) were not audited for the same last-component gap.
- Deploy: not on the ryzen/trashcan kernels yet.
