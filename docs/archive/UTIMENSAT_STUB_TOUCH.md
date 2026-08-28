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

- `touch file` — creates the file. Works.
- `touch -d <date>` / `touch -t <stamp>` — succeed and change nothing.
- **Plain `touch` on an *existing* file does not refresh its mtime either.**
  That is the consequential one: `touch` as a "mark this newer" idiom is inert,
  so `make` and anything else comparing mtimes sees whatever ext2 recorded at
  write time and will not rebuild.

Measured in the guest 2026-08-28, one file through all three forms — the mtime
never moves off the write time:

```
$ echo x > /tmp/ts1; ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Aug 28 20:18 /tmp/ts1

$ touch -d '2001-01-01 00:00:00' /tmp/ts1; echo rc=$?   → rc=0
$ touch -t 200202020202 /tmp/ts1;          echo rc=$?   → rc=0
$ touch /tmp/ts1;                          echo rc=$?   → rc=0
$ ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Aug 28 20:18 /tmp/ts1                 ← unchanged by all three

$ date -u '+%Y-%m-%d %H:%M:%S'          → 2026-08-28 20:18:58
$ busybox stat -c '%y  %Y' /tmp/ts1     → 2026-08-28 20:18:57  1787948337
```

Every call returns 0, which is the honest half of the trade: the *error*
contract is now correct, and the write side is a stub that says so here rather
than in a comment nobody reads.

Returning `ENOSYS` instead would be the honest alternative and is worse: it puts
`touch` back to failing outright, which is the state this replaces.

### OPEN: actually storing the timestamps

Deliberately not folded into this fix. What it needs, in order:

1. **A set-times operation on `akuma_vfs::Filesystem`.** There is none today —
   the trait is read-only with respect to time, which is why the handler has
   nowhere to put the values. Signature has to carry `UTIME_OMIT` (leave this
   one alone) rather than two plain `u64`s, or `touch -a` will clobber mtime.
2. **ext2 inode writeback for `i_atime` / `i_mtime` / `i_ctime`.** The fields
   exist on disk and are already read; nothing writes them back.
3. **`Metadata.modified` / `.accessed` stop being read-only**, and `sys_utimensat`
   drops its "discard" branch.

Whoever picks this up should extend `test_utimensat` with the read-back case it
deliberately omits today, and re-run the mtime ladder under "What is still not
implemented" — it is written to be re-run and should start disagreeing with
itself when this lands.

Worth doing when something in the guest actually depends on mtime. The concrete
trigger is a build system: `make` inside the VM will not rebuild on `touch`,
and self-hosted builds are the direction this OS is going.

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

Confirmed on both platforms at SMP=4: QEMU **316 PASSED / 0 FAILED**, and
Firecracker under Lima **308 PASSED / 0 FAILED / 0 POISON**, with
`[Test] utimensat PASSED (10 cases)` on each.

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
