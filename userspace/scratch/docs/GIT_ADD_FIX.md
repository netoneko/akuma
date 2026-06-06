# Git Add Staging Bug Fix

## Summary

Fixed two bugs that caused `scratch add <file>` to report "staged 0 file(s)" even when the file existed and should have been staged.

## Bug 1: read_dir returning true for regular files

**Location:** `libakuma/src/lib.rs` - `ReadDir::open()`

**Problem:** The `read_dir()` function only checked if `open()` succeeded, but `open()` succeeds on regular files too. This caused `scratch add` to incorrectly treat files as directories.

**Symptom:**
```
scratch add meow.js
DEBUG add_path: path=meow.js abs_path=/meow/meow.js is_dir=true
scratch: staged 0 file(s)
```

**Fix:** Added `fstat()` check to verify the opened file descriptor is actually a directory:

```rust
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;

if let Ok(stat) = fstat(fd) {
    if (stat.st_mode & S_IFMT) != S_IFDIR {
        close(fd);
        return None;
    }
}
```

## Bug 2: Incorrect staging count for existing files

**Location:** `scratch/src/index.rs` - `add_path()`

**Problem:** The staging count was calculated as `entries.len()` after minus before. When updating an existing file in the index, `BTreeMap::insert()` replaces the entry without changing the length, so the count was 0.

**Fix:** Changed `add_file()` and `add_directory()` to return the actual count of files staged (1 per file), rather than inferring it from the map length change.

## Files Modified

- `userspace/libakuma/src/lib.rs` - Fixed `ReadDir::open()` to check file type
- `userspace/scratch/src/index.rs` - Fixed `add_file()`, `add_directory()`, and `add_path()` return values

## Testing

```bash
# Before fix
scratch add meow.js
scratch: staged 0 file(s)

# After fix
scratch add meow.js
scratch: staged 1 file(s)
```
