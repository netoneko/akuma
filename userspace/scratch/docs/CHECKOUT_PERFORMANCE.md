# Checkout performance

## Symptom

`scratch clone` on large repos (e.g. akuma, ~3900 objects, ~500 files) becomes
very slow during the "checking out files" phase. The bottleneck is visible on
deeply nested paths like `akuma/userspace/sqld/sqlite3/`.

## Root cause

Every kernel file operation (`open`, `read`, `write`, `mkdir`) resolves the full
path from the filesystem root on every call. There is no inode cache or
directory-entry cache.

For a single file at path `akuma/userspace/sqld/sqlite3/shell.c`, checking it
out requires:

1. **store.read(sha)** — read the compressed git object:
   - `open(object_path)` → ext2 path lookup (4+ directory reads)
   - 1–2 `read` calls → ext2 path lookup each time + block reads
   - `close`

2. **open(dest, O_CREAT|O_TRUNC)** — create the output file:
   - `exists(path)` → ext2 path lookup
   - `exists(parent)` → ext2 path lookup
   - `write_file(path, &[])` for O_TRUNC → ext2 path lookup + inode alloc

3. **write(fd, content)** — write file data:
   - `write_at(path, 0, data)` → ext2 path lookup + block alloc + writes

4. **close(fd)**

That is ~6 full path resolutions per file, each traversing directory blocks via
VirtIO I/O. For 500 files with average depth 4, that is ~12,000 directory block
reads from the virtual disk.

## Mitigations applied

- **O(n²) sys_read fixed**: `sys_read` now uses `read_at` (block-level reads)
  instead of `read_file` (reads entire file into memory every call).
- **Pack parsed in memory**: the pack is downloaded into a `Vec<u8>` and parsed
  directly, avoiding the O(n²) sys_read on a temporary file.
- **Progress dots**: checkout prints a dot every 50 files and for large blobs
  (>64 KB) so the user can see it is not hung.

## What would help further (not yet implemented)

- **Inode / dentry cache** in the kernel VFS — avoid re-reading directory blocks
  for repeated path lookups to the same parent directories.
- **fd-based I/O** — have `sys_read` / `sys_write` operate on an inode number
  stored in the file descriptor instead of re-resolving the path string.
- **Batch checkout** — read all needed blobs, sort by directory, create dirs
  once, then write files. Reduces redundant mkdir + path resolution.
