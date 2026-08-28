# `utimensat` was a stub, so `touch` silently created nothing (FIXED 2026-08-28)

`touch newfile` exited **0** and created no file. `echo hi > newfile` worked
fine on the same path in the same shell, which is what made this read as a
filesystem bug rather than a missing syscall.

```
$ touch /tmp/x2; ls /tmp/x2
ls: /tmp/x2: No such file or directory
$ touch /tmp/x2; echo rc=$?
rc=0                                 # <- claims success

$ echo hello > /tmp/x3; cat /tmp/x3
hello                                # <- ordinary create+write is fine
```

## Root cause

One line in the dispatch table:

```rust
nr::UTIMENSAT => 0,                  // src/syscall/mod.rs
```

A bare constant. Every `utimensat` call succeeded, including one naming a path
that does not exist.

That is not a harmless approximation, because of how the canonical caller is
written. busybox `touch` does not stat first — it tries to stamp the file and
treats `ENOENT` as "so create it":

```c
if (utimensat(AT_FDCWD, *argv, ts, 0) != 0) {
    if (errno == ENOENT) {           /* file doesn't exist, create it */
        fd = open(*argv, O_RDWR | O_CREAT, 0666);
        ...
    }
}
```

Against a stub that always returns 0, the `if` never fires. `touch` believes it
stamped an existing file, skips the create, and exits 0. **The stub's success
return is what suppressed the file creation.**

## Why it hid for so long

Three things kept it quiet:

1. **It only breaks `touch`-shaped callers** — programs that use a syscall's
   *error* as control flow. Anything that opens with `O_CREAT` directly (every
   shell redirect, every compiler, `cp`, `tar`) never consults `utimensat`.
2. **The failure has no error message anywhere.** Exit status 0, empty stderr,
   no kernel log line. There is nothing to grep for.
3. **It presents as data loss, not as a missing feature.** The first reading is
   "the write didn't persist" — which sends you into the ext2 cache, the disk
   image, and snapshot mode before you get anywhere near the syscall table. That
   is exactly the wrong direction, and it is where this was first chased.

The tell that redirects you: `echo > file` works and `touch file` does not. Both
create; only one goes through `utimensat`.

## The fix

`fs::sys_utimensat` (`src/syscall/fs.rs`), dispatched at `nr::UTIMENSAT`. The
**errors** are now real, because errors are what callers branch on:

| condition | answer |
|---|---|
| path does not exist (after symlink resolution) | `ENOENT` |
| bad `dirfd` — negative and not `AT_FDCWD`, or absent from the fd table | `EBADF` |
| `path == NULL` (`futimens`) with an fd that is not open | `EBADF` |
| `times` pointer unreadable | `EFAULT` |
| `tv_nsec` neither `0..1e9` nor `UTIME_NOW`/`UTIME_OMIT` | `EINVAL` |
| undefined bit in `flags` | `EINVAL` |
| otherwise | `0` |

Argument validation happens **before** the path lookup, matching Linux: a
malformed `times` is `EINVAL` even when the path would also have been `ENOENT`.
The dirfd ladder is shared with every other `*at` syscall via `fs::dirfd_base`.
`UTIME_NOW` / `UTIME_OMIT` were added to `akuma-syscalls-linux`
(`flags::utimensat`) with a host test pinning their values, rather than spelled
locally.

## What is still not implemented, and why that is not hidden

**The timestamps are discarded.** `akuma_vfs::Filesystem` has no set-times
operation at all, and `Metadata`'s `modified`/`accessed` are read-only, so there
is nowhere to put them. Consequences:

- `touch file` — works.
- `touch -d '2001-01-01' file` — succeeds and does not change the date.
- `make`, and anything else that compares mtimes, sees whatever ext2 recorded on
  write, not what `touch` asked for.

Returning `ENOSYS` instead would be the honest alternative and is worse: it puts
`touch` back to failing outright, which is the state this replaces. Storing the
times properly means adding a write path to the `Filesystem` trait plus ext2
inode writeback — a real change, deliberately not folded in here.

## Verify

Boot test `test_utimensat` (`src/process_tests.rs`, 10 cases) covers the table
above. Case 2 is the bug itself; the rest exist so a future "simplification"
back to a constant fails loudly. It deliberately does **not** assert that
timestamps were stored — there is nothing to read back, and a test claiming
otherwise would be asserting a fiction.

End to end in the guest:

```
$ rm -f /tmp/t1; touch /tmp/t1; echo rc=$?; ls -l /tmp/t1 | wc -l
rc=0
1                                    # created

$ cd /tmp && rm -f t2 && touch t2 && ls t2
t2                                   # relative path

$ touch -c /tmp/never_here; echo rc=$?; ls /tmp/never_here
rc=0
ls: /tmp/never_here: No such file or directory   # -c must NOT create

$ touch /nonexistent_dir/x; echo rc=$?
touch: /nonexistent_dir/x: No such file or directory
rc=1
```

`touch -c` is the case that proves the `ENOENT` is doing real work rather than
being swallowed: busybox suppresses the create for `-c`, so the file must stay
absent while the command still exits 0.

## How it was found

Incidentally, while verifying an unrelated `*at`-syscall refactor: a guest
smoke-test did `cd /tmp && touch dirfd_probe && ls dirfd_probe` and the `ls`
failed. The first hypothesis was that the refactor had broken relative-path
resolution. Two checks killed that:

1. **Absolute paths failed the same way** (`touch /tmp/abs_probe`), and the
   absolute case does not go through dirfd resolution at all.
2. **A build from before the refactor behaved identically**, which ruled out the
   change entirely.

Only then did narrowing it to `touch` specifically — rather than "writes" —
point at the syscall. The general lesson is the second check: A/B against a
pre-change binary *before* forming a theory about which of your own edits did it.

## Background

- `docs/reference/abi/linux-compat.md` — errno encoding and the two
  `Result<_, u64>` sign conventions the new handler has to respect.
- `docs/reference/subsystems/syscalls/` — the per-family syscall reference set.
- `dirfd_base` in `src/syscall/fs.rs` — the shared `*at` dirfd ladder this
  handler resolves through, and the four-way divergence it replaced.
