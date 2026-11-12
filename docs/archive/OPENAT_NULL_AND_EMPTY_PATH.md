# `openat(NULL)` and `openat("")` opened the current directory (2026-11-12)

**Status: fixed in glue, for both kernels. Found when amd64's `openat` moved to glue.**

## Symptom

The amd64 boot check `fd: a null path is EFAULT` got fd 14 instead of `EFAULT`, and the next
check, which counts a full table, came up one short (the leaked fd).

## Cause

Two defects, one exposing the other:

1. `copy_from_user_str` refused `ptr < 0x1000` only when `BYPASS_VALIDATION` was off. The boot
   tests run with it on (they pass kernel buffers), and on amd64 ring 0 reads page 0 as zeros, so
   a NULL path read as `""`.
2. `openat_path` resolved `""` against the dirfd, which names the dirfd itself. `open("")` from
   a program, with no bypass involved, opened the cwd on both kernels. Linux says `ENOENT`
   (`openat` has no `AT_EMPTY_PATH`).

amd64's old `openat` used its own `uaccess::read_cstr`, which refuses 0 outright. That is why
the first defect had never shown there.

## Fix

- `copy_from_user_str` refuses page 0 unconditionally. No kernel buffer lives there.
- `openat_path` answers `ENOENT` for an empty path, after the `O_TMPFILE` check and before the
  `RLIMIT_NOFILE` check (Linux's order).

## Verified

`userspace/forktest/c_stress/fdlimit.c` rows `open_null` (`-14`) and `open_empty` (`-2`) are
identical on Linux 7.0, the AArch64 kernel and amd64. Boot checks `fd: a null path is EFAULT`
and `fd: an empty path is ENOENT` pass.
