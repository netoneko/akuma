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

## Storing the timestamps (landed the same day)

The first version of this fix made the *errors* correct and discarded the
timestamps, because `akuma_vfs::Filesystem` had no way to set them. That section
is preserved below as the "OPEN" item it was, because the estimate in it was
wrong in an instructive way.

**The claim was that ext2 needed inode writeback for the time fields. It did
not.** `Inode` already carries `access_time` / `creation_time` /
`modification_time` (`crates/akuma-ext2/src/ext2.rs:809`); they are set on
create (`:2574`, `:2626`) and refreshed on write (`:2254`); `write_inode`
(`:1690`) persists them; and `metadata` surfaces all three into `Metadata`
(`:2736-2738`). That is why `ls -l` always showed a real mtime — the one from
the last *write*. The whole gap was one missing trait method.

What landed:

1. **`Filesystem::set_times(path, atime_secs, mtime_secs)`**, defaulting to
   `NotSupported`. Two `Option`s, not two `u64`s: `None` is `UTIME_OMIT`, and
   without it `touch -a` would have to read-modify-write and clobber mtime.
   `UTIME_NOW` is resolved by the syscall layer, which owns the clock, so no
   filesystem has to.
2. **ext2 implements it** — 8 lines, modelled on `chmod` directly above it, and
   deliberately *not* bumping mtime when only atime is given (`chmod` does bump
   it, because changing a mode changes the inode; `touch -a` must not).
3. **memfs implements the mtime half.** It has no access-time field at all, so
   atime is accepted and dropped — documented at the impl.
4. **`sys_utimensat` resolves `UTIME_NOW`/`UTIME_OMIT` and calls through.**

Two edges worth knowing:

- **A clock that was never set leaves the stamps alone** rather than writing 0.
  On a platform with no RTC and no SNTP yet, "now" is unknown; dating every
  touched file to 1970 would be worse than not touching them.
- **`NotSupported` and `NotFound` from `set_times` are both success.** Existence
  is already established before the call, so those two mean "nothing to stamp" —
  every synthetic path `vfs::exists` knows about (device nodes, `/dev`,
  `/proc/mounts`) has no inode and no mount to hand the request to. The boot test
  caught this: an earlier version accepted only `NotSupported`, and
  `touch /dev/null` regressed to `ENOENT`.

### Measured after (same file, same ladder as before)

```
$ echo x > /tmp/ts1; ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Aug 28 21:02 /tmp/ts1

$ touch -d '2001-01-01 00:00:00' /tmp/ts1; ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Jan  1  2001 /tmp/ts1        ← was: unchanged

$ touch -t 0202020202 /tmp/ts1; ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Feb  2  2002 /tmp/ts1        ← was: unchanged

$ touch /tmp/ts1; ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Aug 28 21:02 /tmp/ts1        ← back to now; `make` sees it

$ touch -d '2003-03-03 03:03:03' /tmp/ts1; stat -c %Y   → 1046660583
$ touch -a /tmp/ts1;                       stat -c %Y   → 1046660583
                                             ↑ UTIME_OMIT: mtime untouched
```

> `touch -t` is spelled in its 10-digit `YYMMDDhhmm` form here rather than the
> 12-digit `CCYYMMDDhhmm` one. Same instant, and it keeps the line out of this
> repo's public-secret scanner: any bare 12-digit run has the shape of an AWS
> account id, and the century-prefixed spelling of this date is exactly twelve
> digits. Allowlisting it would have worked too;
> `scripts/cloud_secret_scan_allow.txt` says to prefer rewording, so the
> heuristic is never weakened for a doc's convenience. (Writing the offending
> literal into the explanation tripped it a second time — hence the wording.)

The three `touch` forms now give three different answers where they previously
gave one. `touch -a` is the case that proves `UTIME_OMIT` is modelled rather
than approximated.

<details>
<summary>The original "not implemented" section, kept for the estimate it got wrong</summary>

**The timestamps are discarded.** `akuma_vfs::Filesystem` has no set-times
operation at all, and `Metadata`'s `modified`/`accessed` are read-only, so there
is nowhere to put them. Consequences:

- `touch file` — creates the file. Works.
- `touch -d <date>` / `touch -t <stamp>` — succeed and change nothing.
- **Plain `touch` on an *existing* file does not refresh its mtime either.**
  That is the consequential one: `touch` as a "mark this newer" idiom is inert,
  so `make` and anything else comparing mtimes sees whatever ext2 recorded at
  write time and will not rebuild.

```
$ touch -d '2001-01-01 00:00:00' /tmp/ts1; echo rc=$?   → rc=0
$ touch -t 0202020202 /tmp/ts1;            echo rc=$?   → rc=0
$ touch /tmp/ts1;                          echo rc=$?   → rc=0
$ ls -l /tmp/ts1
-rw-rw-rw- 1 0 0 2 Aug 28 20:18 /tmp/ts1                 ← unchanged by all three
```

Returning `ENOSYS` instead would be the honest alternative and is worse: it puts
`touch` back to failing outright, which is the state this replaces.

**The estimate this section carried was wrong**, and that is the reason to keep
it: it said the work needed "ext2 inode writeback for `i_atime`/`i_mtime`/
`i_ctime`" and that "nothing writes them back". The field names were the C ones,
not this tree's, and the writeback already existed. Checking the claim before
acting on it turned a multi-layer job into one trait method — the general
lesson being to verify a "what's left" list against the code before scheduling
it, not after.

</details>

## Verify

Boot test `test_utimensat` (`src/process_tests.rs`, 11 cases) covers the table
above. Case 2 is the bug itself; case 11 is the read-back — it sets an explicit
mtime with `UTIME_OMIT` on atime and asserts the value comes back through
`vfs::metadata`, so "stored" is checked rather than assumed. The rest exist so a
future "simplification" back to a constant fails loudly.

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

Confirmed on both platforms, with `[Test] utimensat PASSED (11 cases: …/readback)`
on each:

| platform | result |
|---|---|
| QEMU, SMP=4, HVF, 2 GB | 317 PASSED / 0 FAILED |
| Firecracker under Lima, 1 vCPU, KVM | 308 PASSED / 0 FAILED / 0 POISON |

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
