# amd64 C1 step 4b, batch 2d: `openat` folds

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `AKUMA_AMD64_4B_FOLD_BATCH2C.md` (one `/proc`), which ends with
the line this batch is: *"Left for `openat`: only the four flags glue does not
enforce and the `fd/0` interception."*

`openat(2)` is `akuma-syscalls-glue`'s arm on this target now. It is the one
that mattered: every other file syscall here reads a descriptor `openat`
produced, so until it folded, `read`, `lseek`, `fstat` and `getdents64` were
folding against a descriptor built by a second implementation.

`amd64/src/fd.rs`: **+248 / −297**. The body that went is the `AT_FDCWD` ladder,
symlink resolution, the `/dev` character nodes, the existence and
parent-directory probes, `O_CREAT`/`O_TRUNC` through `write_file`, the
`O_APPEND` seed, and the descriptor allocation. What replaced it is a preamble
of four parts and one call.

## The preamble, and why each part cannot be anywhere else

1. **The flag hop.** The word arrives in the x86_64 encoding; four bits are a
   *permutation* against asm-generic (`O_DIRECTORY`↔`O_DIRECT`,
   `O_NOFOLLOW`↔`O_LARGEFILE`). `akuma_syscalls_abi::open_flags::x86_64_to_aarch64`
   re-encodes it once, at the boundary, exactly as `Syscall::from_x86_64` does
   for the number one argument along.
2. **`/proc/<pid>/fd/0`.** The last path this kernel answers for itself, for the
   semantic reason batch 2c states: `sshd`'s bridge wants the *write end of that
   child's stdin pipe*, and the shared sink delivers into a `StdioBuffer` this
   target's children never read. Unchanged, and still asked of the raw path.
3. **Three flags and a directory guard glue does not enforce** — `O_DIRECTORY`,
   `O_EXCL`, `O_NOFOLLOW`, `O_CREAT`-on-a-directory. See below.
4. **A block node is `ENODEV`.** Glue serves `/dev/vdX` as a `BlockDev`
   descriptor (`proposals/RAW_BLOCK_DEVICE_FD.md`); nothing here reads one, and
   `akuma_virtio::block` *does* have `vda` registered on this target, so a
   folded `open("/dev/vda")` would have handed back a descriptor whose first
   `read` answers `EBADF` — a failure at the wrong syscall, which is the
   `O_TMPFILE` lesson exactly. The self-test that asserts `ENODEV` is what
   caught it, before any of this reached a guest.

Everything else is `akuma_syscalls_glue::fs::openat_path`.

## The seam: `sys_openat` splits in two

Glue's arm was one function that copied the user string and then did the work.
A preamble in another kernel needs the path *before* the arm runs, so the copy
and the work are two functions now: `sys_openat(dirfd, path_ptr, …)` copies and
calls `openat_path(dirfd, raw_path: &str, …)`. Mechanical, no behaviour change,
and it is what stops this target reading the user string twice per `open(2)`.

`resolve_path_at` became `pub` for the same reason: the preamble's refusals have
to be asked of *the path the arm will open*, and a second `AT_FDCWD` ladder in
another kernel is the drift that function was written to end. Handing the
resolved absolute path back to `openat_path` costs a `canonicalize` and no
lookup — `dirfd_base` returns before it touches the table.

## The three flags: kept here, and what moving them would cost

`O_DIRECTORY`, `O_EXCL` and `O_NOFOLLOW` are Linux's answers and they belong in
glue, which enforces none of them: it always resolves symlinks, ignores
`O_EXCL` on an existing file, and hands a regular file to
`open(…, O_DIRECTORY)`. Moving them is a **behaviour change on the AArch64
kernel**, and the AArch64 verification loop does not run on this machine —
reproduced again on 2026-09-10 while writing this: under HVF the boot asserts in
QEMU (`hvf_handle_exception … isv`) partway through the user-copy self-tests,
and under TCG it panics in a pre-existing test (`test_spawn_ext_passes_env`,
Open issue 4 in the parent document). Batch 2a recorded the same finding and
made the same call. So they stay in the preamble, stated as a divergence rather
than assumed absent, and the day that loop works they are three small moves with
a probe already written to check them.

`O_NOFOLLOW` did change here, and for the better. It used to mean "skip the
symlink walk", which is the weaker half of the flag and produced `ENOENT` (the
walk skipped, the existence probe then run against a link inode ext2 reports as
`NotAFile`). Glue resolves unconditionally, so the choice was between Linux's
`ELOOP` and silently following a link the caller asked not to follow.
`akuma_vfs_glue::is_symlink` is the exact predicate: `resolve_symlinks` reads the
link off the *whole* path and never walks intermediate components, so "the path
glue would rewrite" and "the final component is a link" are the same question in
this tree. `errno::ELOOP` is new to this file.

## What the fold gained, measured from ring 3

- **`mode` reaches the filesystem.** The old arm took it as `_mode` and threw it
  away, so every file this target created came out with whatever `write_file`
  picked. Glue `chmod`s a created file to `mode & 0o7777`.
- **A bogus negative `dirfd` is `EBADF`**, not `ENOTDIR` — glue's ladder is the
  one that refuses `openat(-5, "rel")` rather than resolving it against `/`.
- **`AT_FDCWD` resolves against `Process::cwd`.** `/` for every process here
  until this target grows `chdir`, and correct on the day it does.
- **Paths up to 1024 bytes**, glue's bound; this file's `path_from_user` reads
  256 and answered `EFAULT` past it.

## The two things that had to move first

**1. The boot row starts with stdio.** Glue allocates with `alloc_fd`, which is
`alloc_fd_from(0)`. Batch 2a gave every registered process the
`SharedFdTable::with_stdio` triple for exactly this reason, but the *boot row* is
built by `akuma_exec::process::make_test_process`, which uses
`SharedFdTable::new()` — so the suite's first `open` returned **fd 0**, and every
check spelled `fd >= FIRST_FILE_FD` read a successful open as a failure.
`boot_row_register` writes the triple now. `make_test_process` is shared with 25
AArch64 call sites and was left alone.

**2. `MAX_FDS` is a lookup bound, not just a budget.** Glue's `alloc_fd` has no
ceiling — a `BTreeMap` grows — and `install` refused at 256. That is not merely a
policy difference here: `table_get_in`, `is_bound` and `is_nonblocking` all
refuse a number at or above `MAX_FDS`, so a descriptor past the ceiling would be
a successful `open` that every later syscall answers `EBADF` for. The boot
suite's `fd: a full table is EMFILE` check found it on the first run: `got 0x100`
— fd 256, handed out and unusable. The preamble asks the ceiling, one lock,
where `install` asks it for everything this module still allocates itself.

## A probe, because the boot suite structurally cannot see this

`userspace/forktest/c_stress/openflags.c` (new): 20 assertions about `open(2)`'s
flag vocabulary, in the house PASS/FAIL/SKIP/DIVERGE shape, runnable on Linux to
prove the probes themselves are right. It exists because the four refusals the
preamble keeps had, until now, exactly one thing asserting them — a kernel-side
self-test running under `BypassValidationGuard`, which is the arrangement that
hid a `CR4.SMAP` bug for a batch (`AKUMA_AMD64_4B_FOLD_BATCH1.md`).

| where | result |
|---|---|
| real Linux (the box's Ubuntu side, same static binary) | 19 PASS, **1 DIVERGE** (`O_TMPFILE`, which a real Linux supports) |
| Akuma/amd64, QEMU | **20 PASS, 0 FAIL** |
| Akuma/amd64, bare metal | **20 PASS, 0 FAIL** |

Every assertion is therefore Linux-correct *and* answered correctly here,
including the two the fold gained (`mode`, the negative `dirfd`).

## The dead witness this batch found, and fixed

`scripts/utils/amd64_ring3_check.py` reads the **kernel heap** either side of its
ssh churn, because `free` is blind to that whole class — the PMM number did not
move at all across the 135 MB excursion into the whole-file `fd` cache
(`proposals/AMD64_FD_WHOLE_FILE_HEAP.md` § "And a method correction"). It read
that number out of `/proc/meminfo`'s `Cached:` row, which amd64's own synthetic
`/proc` rendered as `akuma_alloc::stats().allocated`.

Batch 2c deleted that view for the mounted `ProcFilesystem`, where `Cached:` is
the *file page* cache — and amd64 has none. **The check has been reporting
`0 -> 0 kB, drift +0` ever since**, which reads as a perfect result and is a
column of zeroes. Nothing failed; that is the point.

The shared render carries the heap under its own Linux name now — `Slab:`,
additive, one row, ignored by busybox `free`'s prefix matching — so the row means
the same thing on both kernels, and the check fails rather than scores when the
row is absent. First live reading in a week: **1576 → 1573 kB across 30 ssh
sessions, drift −3 kB**, on the kernel this batch produced.

## Verification

| gate | before 2d | after |
|---|---|---|
| QEMU/TCG `SMP=1` / `SMP=4` | 579/0 / 589/0 | **579/0 / 589/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 565/0 / 575/0 | **565/0 / 575/0** |
| bare metal | 579/0 | **579/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK (heap column dead) | **OK, heap −3 kB of a real reading** |
| `openflags` probe (QEMU / metal / Linux) | — | **20/20 · 20/20 · 19 + 1 known** |
| `apk update` + `apk add file` (QEMU + metal) | OK | **OK**, `file-5.47` runs |
| clippy, both kernels; host tests | clean | **clean** |

The check counts are unchanged on purpose: the suite's `openat` section is the
same section it always was, now asserting against the folded arm. It passing
unchanged is the statement that the two implementations agreed about everything
it asks.

## Left for the next batch

`read`, `pread64`, `write`, `lseek`, `fstat` and `getdents64` are the cluster
that reads what `openat` now produces, and they are the natural next fold. Two
things they will meet:

- **The `/dev` variants.** Glue hands out `DevNull`/`DevZero`/`DevUrandom`/
  `DevTty` where this file used to hand out a `File` carrying the node's path;
  `dev_node_of` maps both spellings to one name, which is what let this batch
  land without touching `read`. A folded `read` inherits glue's arms instead.
- **`/dev/tty` is `ENODEV` here now.** Glue requires a terminal `channel` and no
  process on this target has one, where the old arm gave back a console
  descriptor. Nothing here opens it (`less`, `vi` would), and telling a pager
  there is no controlling terminal is a better answer than handing it the serial
  line of a machine it is not sitting at — but it is a change, so it is written
  down.

## Background

- `docs/archive/AKUMA_AMD64_4B_PREREQUISITES.md` — slices A–C, and the `open(2)`
  flag permutation this batch's hop depends on.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH1.md` — the five path-only arms, and
  the facade gate every batch has to ask about.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2{A,B,C}.md` — the alignments, `close`,
  and one `/proc`.
- `docs/archive/APK_OTMPFILE_DIR_FD.md` — the bug the `O_TMPFILE` refusal exists
  to prevent.
