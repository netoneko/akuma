# getdents64 Directory Cache Fix

## Symptom

`rm -rf` on directories with many entries (e.g. `/.cache/go-build` with 256 subdirectories) fails with `Directory not empty`, requiring multiple retries before succeeding:

```
akuma:/playground> rm -rf /.cache/go-build/
rm: can't remove '/.cache/go-build': Directory not empty
akuma:/playground> rm -rf /.cache/go-build
rm: can't remove '/.cache/go-build': Directory not empty
akuma:/playground> rm -rf /.cache/go-build
akuma:/playground>
```

The number of retries needed scales with directory size. Both the built-in shell `rm` and userspace `rm` (via musl) exhibited the behavior.

## Root cause

`sys_getdents64` re-listed the directory from disk on every call and used an index-based `skip(f.position)` to resume where the previous call left off. When `rm` deletes entries between calls, the entry list shrinks, and `skip(position)` jumps past entries that shifted forward in the list.

Concrete example with a 256-entry directory and a ~64-entry readdir buffer:

1. **Call 1**: entries = `[00..ff]` (256), returns `[00..3f]`, position = 64. `rm` deletes `00`-`3f`.
2. **Call 2**: entries = `[40..ff]` (192 remaining), `skip(64)` starts at entry `80`. Entries `40`-`7f` are **never returned**.
3. Eventually `position >= entries.len()` → returns 0. `rm` tries `rmdir` → `ENOTEMPTY` because `40`-`7f` still exist.

Each `rm -rf` attempt roughly halves the remaining undeleted entries, matching the observed "fails twice, succeeds on third try" pattern.

## Fix

Added a directory entry cache (`dir_cache`) to `KernelFile`. On the first `getdents64` call, the directory listing is snapshotted into the fd. Subsequent calls return entries from the snapshot, so deletions between calls no longer shift index positions.

The cache is cleared when `lseek` resets the position to 0 (i.e. `rewinddir`), allowing a fresh snapshot on the next enumeration.

### Files changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/types.rs` | Added `DirCacheEntry` struct and `dir_cache` field to `KernelFile` |
| `src/syscall/fs.rs` | `sys_getdents64`: populate and use the cache; `sys_lseek`: clear cache on seek to 0 |

### Previous behavior

```
getdents64(fd, buf, size):
    entries = list_dir(path)           // re-read from disk every call
    for entry in entries.skip(position):
        write entry to buf
        position += 1
```

### New behavior

```
getdents64(fd, buf, size):
    if fd.dir_cache is None:
        fd.dir_cache = list_dir(path)  // snapshot once
    for entry in fd.dir_cache.skip(position):
        write entry to buf
        position += count
```

## Related

The disk image also has the `dir_index` ext2 feature enabled (checked via `tune2fs -l disk.img`), but no directories on disk actually use htree (`Flags: 0x0` on all checked inodes). All directories were created by Akuma's ext2 driver which uses flat linear layout. The `dir_index` feature flag is harmless as long as no htree directories are encountered, but if a Linux tool were to create large directories on the image, Akuma's linear `parse_directory` would misparse the htree blocks.
