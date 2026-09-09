# amd64 C1 step 4b, prerequisites: the open-flag vocabulary

**Date:** 2026-09-09
**Status:** slices A, B and C landed, uncommitted. QEMU/TCG `SMP=1` **546/0**,
`SMP=4` **556/0**, ring-3 30/30, memory probes 8/10 (0 unexpected), host tests
+7.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `docs/archive/AKUMA_AMD64_C2_SLICES_6_AND_7.md`.

4b is the fold that retires `amd64/src/fd.rs` — 22 syscall arms in the
dispatcher still go to it rather than to `akuma-syscalls-glue`'s `fs.rs`. This
document is what has to be true *before* an arm moves, found the way C1 step 1's
was: by reading what a folded arm would actually touch.

---

## Slice A — `open(2)`'s flags are a second vocabulary, and it is permuted

C1 step 1 exists because a syscall **number** means different things on the two
architectures, and handing glue the wrong one is a wrong answer rather than a
compile error. The same is true one argument along, and nothing had said so:
aarch64 Linux keeps the **32-bit ARM** fcntl values, so four `open(2)` flag bits
are a *permutation* between the two architectures.

Read out of this tree's own vendored musl headers
(`userspace/tcc/vendor/musl-dev-{aarch64,x86_64}.apk`, `bits/fcntl.h`) — the
right source, because the question is not what a kernel header says but what the
libc in the guest passes:

| bit | aarch64 | x86_64 |
|---:|---|---|
| `0o40000`  | `O_DIRECTORY` | `O_DIRECT` |
| `0o100000` | `O_NOFOLLOW`  | `O_LARGEFILE` |
| `0o200000` | `O_DIRECT`    | `O_DIRECTORY` |
| `0o400000` | `O_LARGEFILE` | `O_NOFOLLOW` |

**Every other `O_*` bit is identical** — `O_CREAT`, `O_EXCL`, `O_NOCTTY`,
`O_TRUNC`, `O_APPEND`, `O_NONBLOCK`, `O_DSYNC`, `O_ASYNC`, `O_NOATIME`,
`O_CLOEXEC`, `O_PATH`, `__O_TMPFILE`. That is what makes it dangerous: a reader
who spot-checks `O_CREAT` and `O_CLOEXEC` concludes the encodings agree, and
`amd64/src/fd.rs` said exactly that in a comment —

> Spelled out locally rather than pulled from `akuma_syscalls_linux` because
> those are the AArch64/`asm-generic` values; on x86_64 they **happen to share
> the same numeric encoding** … naming that coincidence explicitly here is
> cheaper than a reader having to go check.

The file *knew better* in three other places, as `O_NOFOLLOW_X86`,
`O_TMPFILE_X86` and `O_EXCL_X86` declared inline at the three sites that needed
them. Each was correct on its own and none contradicted the comment out loud,
which is how a coincidence survives as a stated fact.

### Why the permutation is worse than a mismatch

It is two transpositions, and **both turn one real flag into another real
flag**, so nothing rejects either direction:

- **`O_DIRECTORY` ↔ `O_DIRECT`.** An x86_64 caller's `O_DIRECTORY` read with the
  aarch64 table is a cache hint, so the "this had better be a directory" check
  never runs. The other way, an ordinary file open acquires a directory
  requirement it never asked for.
- **`O_NOFOLLOW` ↔ `O_LARGEFILE`.** glibc sets `O_LARGEFILE` on almost
  everything, so this one reads as "do not follow symlinks" on a caller that
  never asked.

And the compound flag inherits it. `O_TMPFILE` is `__O_TMPFILE | O_DIRECTORY`,
so it is `0o20040000` on aarch64 and `0o20200000` on x86_64. Glue's `sys_openat`
**refuses** `O_TMPFILE` deliberately — apk-tools 3 probes for it, and an open
that succeeds and then discards the bytes surfaced as `UNTRUSTED signature` over
a download that was fine (`docs/archive/APK_OTMPFILE_DIR_FD.md`). Fed an
untranslated x86_64 word that refusal **does not fire**: `0o20200000 &
0o20040000` is `0o20000000`, not the mask, so the guard tests false and a bug
this tree has already paid for once comes back on the architecture that never
had it.

### This is not a live bug today, and that is the point

Nothing is broken on the current kernel: the three inline `_X86` constants named
the right bits, so `sys_openat` was self-consistent. **The defect is scheduled
rather than present** — it arrives the moment a folded glue arm owns the
refusal, and it arrives silently. That is the same shape as step 1's syscall
numbers, and the same remedy: translate once at the boundary, before anything
depends on it.

### What landed

`akuma_syscalls_abi::open_flags`, in the crate that already exists to answer
"which encoding, on which architecture":

- `x86_64::{O_DIRECT, O_LARGEFILE, O_DIRECTORY, O_NOFOLLOW, O_TMPFILE}` as
  literals, because this crate owns the x86_64 table;
- `aarch64::{…}` re-exporting `akuma_syscalls_linux::flags::open` for the three
  that crate names, so they cannot drift, and declaring `O_DIRECT` /
  `O_LARGEFILE` locally because it does not — its rule is that a constant
  appears when a caller needs one, and no AArch64 code reads either. **They are
  needed here even so**: a translation that passed them through would leave each
  meaning the other flag, which is the whole defect.
- `x86_64_to_aarch64` / `aarch64_to_x86_64`, written out separately rather than
  aliased. They are the same function today because a permutation of
  transpositions is its own inverse, and a test pins that — but leaning on it
  silently would let a future bit that is *not* self-inverse break one direction
  with nothing to say which.

`amd64/src/fd.rs`'s `sys_openat` re-encodes the word **once, at its own
boundary**. Everything below — the file, `KernelFile::flags`, and whatever glue
arm folds next — then speaks one encoding. The local `open_flags` module is
`pub use akuma_syscalls_linux::flags::open`, and the three `_X86` constants are
gone.

### Seven host tests, two of which assert the defect

The two that matter are negative:

- `untranslated_o_tmpfile_defeats_the_guard` asserts `asked & guard != guard`
  for the *untranslated* word — i.e. it fails if the bug ever stops being real —
  and then that the translation restores the refusal.
- `exactly_four_bits_are_permuted` walks all 32 bits and pins the moved set,
  because the assumption a reader makes here is "none of them move".

Plus the round trip over every bit, the involution, and a cross-check of the
aarch64 halves against `akuma-syscalls-linux`.

### And a live gap the translation let me close

`O_DIRECTORY` was **never read at all** on this target: it had no entry in the
local flag module, and `is_dir` is decided by `fs::metadata`, so
`open("/bin/busybox", O_RDONLY|O_DIRECTORY)` returned a working descriptor on a
regular file. It could not simply have been added, either — the bit ring 3 sets
is `0o200000`, which in the tree's own constant is `O_DIRECT`, so reading it
with `open_flags::O_DIRECTORY` would have tested the wrong bit and adding a
fourth inline `_X86` const would have deepened the split. It is `ENOTDIR` now,
which is Linux's answer and the errno `find` and `ls -R` act on.

### The negative control found a bug in the fix

Five boot checks, every constant in them spelled in the **x86_64** encoding on
purpose — they are what a musl-linked guest sets, and proving the hop happened
is their whole job. Each was falsified against a kernel with the translation
line removed, and two came back red as intended:

```
fd: O_DIRECTORY on a regular file is ENOTDIR  [FAIL] got 0x3   want 0xffffffffffffffec
fd: O_TMPFILE is EINVAL, not EISDIR           [FAIL] got EISDIR want EINVAL
```

`got 0x3` is a **live file descriptor** — the pre-fix answer, for every caller
that ever passed the flag.

The first run of that control did something more useful. It reported `ENOENT`
where the broken arm should have opened a file, which said the fixture
(`/etc/passwd`) is not on the image at all — and therefore that the *passing*
arm had been answering `ENOTDIR` for a **missing** path. The `O_DIRECTORY`
refusal was above the existence probe, so `open("/no/such/path", O_DIRECTORY)`
blamed the directory-ness of something that was not there. Moved below the
probe, fixture changed to `/bin/busybox`, and a sixth check pins the ordering.

**A check whose negative control passes for the wrong reason is not a control.**
The only thing that caught this was reading *why* the red arm was red.

### Verification

| gate | baseline | now |
|---|---|---|
| QEMU/TCG `SMP=1` | 533/0 | **538/0** |
| QEMU/TCG `SMP=4` | 543/0 | **548/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** — 30/30, `free` unmoved, heap +41 kB (tolerance 8192), `grandfork` ALL PASS |
| host tests, `akuma-syscalls-abi` | 8 | **15** |
| clippy (`akuma-amd64`, `akuma-syscalls-abi`) | clean | clean |

**AArch64 cannot be affected, by construction rather than by measurement:** the
diff is `amd64/` plus `akuma-syscalls-abi`, which the root kernel does not
depend on — the workspace lists it as a member and only `amd64/Cargo.toml` links
it.

Ring-3 witnesses on QEMU: `find /etc -type f`, `ls -R /etc`, `>` truncating a
longer file to two bytes, `: >` leaving a zero-byte file, `echo hi > /dev/null`,
and stderr reaching the session across `sh -c '…' > file` — the slice 5/6/7
behaviours, unchanged.

**Also re-measured, and it closes a roadmap issue:** `echo A > /nosuchdir/file`
answers `/bin/sh: can't create /nosuchdir/file: nonexistent directory`, rc 1.
That is open issue 1 of `AKUMA_SELF_HOSTING_AMD64.md`, reported as `rc 0` and
silence; C2 slice 5 closed it and nobody had re-run it.

---

## Slice B — the descriptor stops carrying `is_dir`, and two live bugs fall out

`akuma-syscalls-glue` has **no `is_dir` field** on a descriptor. It refuses a
write open of a directory at `open(2)`, and otherwise lets the VFS answer:
`read` on a directory reaches ext2, which says `NotAFile`, which its errno table
maps to `EISDIR`. amd64 carried a bool instead — set by five constructors, read
by eight sites — so a folded arm would have had nowhere to get it from. The
crate closes the gap for free; this slice takes the shape.

What replaced it is one function, `path_is_dir`, asked of `proc_is_dir` for a
`/proc` path and of the VFS for everything else. It is asked on the *cold* paths
only — `openat` with a real `dirfd`, `fstat`, `mmap`'s regular-file test — and
the hot paths lost their check entirely, because the guard they had only ever
fired on the error they now get from the filesystem anyway.

### 1. `fstat` on a directory descriptor answered `EBADF`

The arm opened `if entry.is_dir { return None }`, and `None` there is `EBADF`.
The `S_IFDIR` arm below it was **unreachable for every ext2 directory**: its
discriminator was *synthetic*, not *directory*, so the only descriptors ever
reported as directories were `/proc` views — which meant `/proc/meminfo` was
reported as a zero-length directory in the same breath.

The consequence is `fdopendir`, which musl builds on exactly this call.
Measured with an x86_64 musl probe **before** the field was touched, because
this file's own header claims the opposite in prose ("a directory descriptor
`S_IFDIR` — musl's `fdopendir` fstats the fd and refuses it with `ENOTDIR`
unless `S_ISDIR` holds, so `ls`/`find` need this to be right"):

```
                                       before                          after
open(/etc, O_DIRECTORY)   3 (ok)                          3 (ok)
fstat(dirfd)              -1 EBADF   mode=00  S_ISDIR=0   0  mode=040755 S_ISDIR=1
fdopendir(dirfd)          NULL (Bad file descriptor)      ok — first entry: apk
```

**busybox never noticed, and that is the interesting half.** It walks with
`opendir(path)` and `lstat`, so `ls`, `find`, `ls -R` and every ring-3 check
this target has ever run went through a path that does not ask. `fdopendir` is
what `nftw` and most `openat`-based directory walkers are built on — the shape a
self-hosting build reaches for. A 538-check boot suite and a 30-session ring-3
harness both had this in front of them the whole time.

### 2. Seven call sites threw the filesystem's error away

`Err(_) => return errno::EIO` stood on the `read`/`pread`/`lseek` paths, and
that is what turned the `is_dir` removal into a *regression* the moment it
landed: `read` on a directory went `EISDIR` → `EIO`, because ext2's `NotAFile`
never reached the mapping. Caught by the ring-3 probe on the same run that
confirmed the `fstat` fix, not by the suite.

`fs_err_errno` is now `akuma-syscalls-glue`'s `fs_error_to_errno` arm for arm —
eleven variants where it had three and a catch-all. Its old comment defended the
catch-all:

> inventing eight more errnos nobody distinguishes is not honesty, it is noise

The argument was sound; the premise was not. Half of them are distinguished by
callers this target already runs — `EISDIR` is how a program learns to call
`getdents64`, busybox `find` reads `ENOTDIR` to stop descending, `EROFS` says
the mount is the problem rather than the disk. And `mkdirat`, `unlinkat`,
`symlinkat` and `utimensat` each carried their **own** two- or three-arm subset
of the same table ending in `EIO` — the same drift `clone_fd_refs`'s header
describes on the other side of the tree: several partial copies of one list,
each correct for the cases its author happened to hit. All four route through
the one table now.

One divergence from glue's table, deliberate and stated: `NotSupported` is
`ENOSYS` here and falls to `EIO` there. `utimensat` is the caller that wants it.

### Also fixed, in passing

- `getdents64` on a regular file answered a blanket `ENOENT` — "no such
  directory" for a path that plainly exists. It is `ENOTDIR`, from `list_dir`.
- `lseek(SEEK_END)` on a directory silently meant `SEEK_SET(0)`: the old
  exclusion left `total` at 0. It takes the real branch now, which is what Linux
  permits.
- `fstat` on a `/proc` render reports `S_IFREG | 0444` and its true size, the
  same answer `newfstatat` gives for the same path.

### Verification

| gate | baseline | now |
|---|---|---|
| QEMU/TCG `SMP=1` | 533/0 | **546/0** |
| QEMU/TCG `SMP=4` | 543/0 | **556/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** — 30/30, `free` unmoved, heap drift −23 kB, `grandfork` ALL PASS |
| `amd64_mem_trials --smp 4` (local) | 8/10, 0 unexpected | **8/10, 0 unexpected** |
| host tests, `akuma-syscalls-abi` | 8 | **15** |
| clippy | clean | clean |

Thirteen checks added across slices A and B, **every one falsified** against the
code it tests: the flag translation removed (2 red), `path_is_dir` forced false
(1 red — `got 0x8000 want 0x4000`, `S_IFREG` where `S_IFDIR` belongs), and the
two shape errnos returned to the catch-all (2 red, both `EIO`).

Firecracker and bare metal not run: the box is on its Ubuntu personality and
rebooting it is a separate, slower step.

### What slice B did *not* do

`Entry` is down to three fields — `desc`, `data`, `nonblocking`, `refs` — and
the flip proper is still ahead:

- **`nonblocking`** has a home (`SharedFdTable::nonblock`) and moving it is not
  a prerequisite for anything: the two are already kept in lockstep by `fcntl`.
  It is worth doing with the flip rather than before it, and it carries a
  divergence to state — the set is keyed per **fd number**, where `Entry` is per
  **description**, so `dup`ping a non-blocking socket loses the flag. That is
  glue's existing behaviour, not a new one.
- **`data`** is the synthetic `/proc` render, and it needs a decision rather
  than a move: re-render per read (what a real procfs does), or a side table.
- **`refs`** is the flip itself — `FILES` stops owning lifetimes,
  `fork_table_mirror` becomes `clone_deep_for_fork`, `clear_table_mirror` goes,
  and `close_all()` becomes the teardown. `flock_release` has to be wired first
  (as a stated no-op — this target dispatches no `flock`), because `close_all`
  fires it for every `File` entry and it is a `not_wired!` panic today.

**A tree-wide divergence found while reading for that, and not yet recorded
anywhere:** `dup` in glue clones the `FileDescriptor` **by value**, so two
descriptors onto one file get **independent cursors**. POSIX says a `dup` shares
the open file description, offset included. amd64's `FILES` refcount gets this
*right* today, so the flip would trade a correct behaviour for a matching one —
which is the sort of thing that has to be a stated decision rather than a side
effect. It is not a regression the flip introduces; it is one the flip would
*adopt*, and it is worth fixing in the crate instead.

---

## Slice C — `flock_release` stops being a landmine with a correct label

One field, and the reasoning is the deliverable. Its `not_wired!` said:

> **this target has no `flock`.** `sys_flock` is not dispatched, nothing takes
> a lock, so nothing can release one. Reaching here means a folded arm brought
> advisory locking with it, and the panic is the notice.

Every clause true. And it was a **panic on the path `SharedFdTable::close_all`
takes for every `File` entry it pops** — which, since C2 slice 4 mirrored real
descriptors into the registered tables, is an ordinary process teardown. The
only thing between that and a dead machine was every exit path remembering
`fd::clear_table_mirror` first; slice 6 found a path that did not, and slice 7
wired the socket hooks for exactly this argument while leaving the `File` arm
alone — because its reason read like a decision.

It was not a decision. It was a landmine with a correct label. The test for a
loud stub is not *"can this be served?"* but **"if this fires, is the panic more
useful than the no-op?"** — and for a teardown hook whose operation is vacuous,
it never is. A release of a lock nothing took is a no-op in any implementation;
the panic could only ever fire on a bug *elsewhere*, and its effect was to
destroy the evidence.

Nine `not_wired!` stubs left, from 16 when the C2 plan was written. This is the
last hook `close_all()` needs before the refcount authority can move at all.

---

## What the flip still needs, and why it is a decision rather than effort

`Entry` is `{desc, data, nonblocking, refs}`. The mechanical part of the flip is
55 touch points inside one file (`with_file` ×20, `FILES.lock` ×18, `FDS.lock`
×8, `file_index` ×9) and no change to the module's external interface. That part
is work. These three are not:

**1. `data` — the synthetic `/proc` render.** There is no field for it in
`FileDescriptor`, and the three candidates are not equivalent:

- *Re-render per read.* Deletes the field, costs a render per `read(2)` on
  `/proc`. Loses the snapshot: Linux's `seq_file` renders **at open** and holds
  the buffer, so a two-read sequence there sees one consistent file and here
  would see two.
- *A side table keyed by (row, fd).* Keeps the semantics, reintroduces the
  second structure the flip exists to remove.
- *Serve `/proc` through the mounted `ProcFilesystem`* — what glue does, and the
  real end state. 5b slice 3 mounted it already, and per-pid `stat`/`status`/
  `cmdline` were **tried and reverted** there: the boot suite runs before
  `run_init`, which is the wall that has now bitten three times.

Note the existing asymmetry: a synthetic *directory* already snapshots into
`KernelFile::dir_cache` and needs nothing. Only the file case is open.

**2. `nonblocking` is per-description; `SharedFdTable::nonblock` is per fd
number.** Moving it is not a prerequisite — `fcntl` already keeps the two in
lockstep — and it *introduces* a divergence: `dup`ping a non-blocking socket
would lose the flag. That is glue's existing behaviour, so the fold arrives
there eventually; it should be a stated decision taken with the flip, not a
tidy-up done before it.

**3. `dup` in glue clones the `FileDescriptor` by value, so two descriptors onto
one file get independent cursors.** POSIX shares the open file description,
offset included. **amd64's `FILES` refcount gets this right today**, so the flip
would trade a correct behaviour for a matching one. Found by reading
`sys_dup`/`sys_dup3`/`clone_deep_for_fork` for the flip, and it appears to be
recorded nowhere in the tree — worth fixing in `akuma-exec` rather than adopting
here, which is a change to the AArch64 kernel and wants its own pass.

## Background

- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — the same problem for
  syscall numbers, and the shape this copies.
- `docs/archive/APK_OTMPFILE_DIR_FD.md` — the bug the `O_TMPFILE` refusal exists
  to prevent, and which an untranslated fold would reintroduce.
- `crates/akuma-syscalls-linux/src/flags.rs` — the module header that names the
  aarch64/asm-generic split. It was right, and it was one crate away from the
  file that assumed the opposite.
