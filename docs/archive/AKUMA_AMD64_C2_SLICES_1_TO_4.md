# amd64 C2, slices 1–5: the whole-file cache is deleted, and the fd table gets a mirror

**Dates:** 2026-09-09. Slices 1–3 verified end-to-end; slice 4 verified on
Firecracker, and its wedge bug found on the metal the same day (see § "The
wedge").
**Plan:** `proposals/NEXT_AGENT_AMD64_C2_FD.md` — retire `amd64/src/fd.rs` in
favour of `akuma-syscalls-glue`'s file surface, sliced, with a boot between
slices. This document records what each slice actually was, including the two
bugs the table work charged and the fix-shaped lesson in each.

---

## Slice 1 — the OOM handler stops halting a BKL-holding core

`proposals/AMD64_FD_WHOLE_FILE_HEAP.md` is the crash: `fd.rs`'s `Entry.data`
cached a whole file in the kernel heap and grew it by `resize`'s doubling, so
one large write needed ~3N of heap at the last doubling, and the amd64
`alloc_error_handler` answered with `halt()` — permanently removing a core
from an `SMP=4` machine behind a single big lock. That is the bare-metal
signature-B ssh lockout, reproduced from a photograph of the framebuffer
before the fix and by the same ladder after it.

Two changes, both in the slice-1 spirit ("a handful of lines, fixes the live
crash"):

- `amd64/src/main.rs`: the OOM handler releases the BKL (`smp::bkl_abandon()`)
  before halting. The allocation that OOMs on this path ran while this core
  held the lock; a halted owner leaves every peer in `[BKL] stuck` forever.
  Releasing it degrades the machine to N-1 cores instead of stopping it.
- `amd64/src/fd.rs`: the write path uses `try_reserve` with exact growth
  instead of `resize`'s doubling; failure returns `ENOMEM` to ring 3, which is
  what Linux does.

**Verified on the metal, where the bug was found.** The ladder
(`cat /bin/akuma` × 120 redirected into one file — one long-lived fd) drove
the file to 134 MB: before, `[OOM] allocation of 268435456 bytes failed` →
`[BKL] stuck` → dead box; after, `cat: write error: Out of memory` — clean
`ENOMEM`, no `[OOM]` line in dmesg at all, ssh serving throughout.

Gates: QEMU/TCG 525/0 (SMP=4; +1 over the 524 baseline is suite growth
elsewhere, not this change), 515/0 (SMP=1), Firecracker 512/0, metal 516/0,
host tests clean, clippy clean in all three configurations.

## Slice 2 — a kernel-heap reading for the ring-3 check

`free` cannot see this whole bug class: across the 135 MB excursion it
reported the same number before and after, because it watches PMM pages and
this is the kernel heap (`AMD64_FD_WHOLE_FILE_HEAP.md` § "And a method
correction").

`/proc/meminfo`'s `Cached:` column is `akuma_alloc::stats().allocated` on this
target (`render_meminfo`), so the kernel heap is already readable from ring 3.
`scripts/utils/amd64_ring3_check.py` now reads it before and after its session
churn (`--heap-tolerance`, default 8 MiB, fails the run on drift).

**The witness, measured on the metal** (sampling races the ladder — a full
120-iteration ladder completes in under a second at ~90 MB/s, so the held
state needs a slow or infinite writer):

| state | `Cached:` |
|---|---|
| baseline | ~1.6 MB |
| fd held open at the ceiling | **132 929 kB → 200 052 kB** |
| after close, no other holders | **1 661 kB** |

That is the whole-file cache, visible to ring 3, coming back on close. (The
200 MB figure is two caches coexisting on one boot; `kill(2)` is not
implemented on this target, so an unkillable `while true` holder kept its
~134 MB live across the later samples.)

## Slice 3 — registration: every process carries a real `SharedFdTable`

5b slice 1 registered a `Process` per pid; its `fds` field was a fresh
`SharedFdTable::with_stdio()`, written nowhere and read by nothing. This
slice made `register_exec_process` take the table as a parameter and
`sys_fork` pass `parent.fds.clone_deep_for_fork()` — the crate-side twin of
the legacy `inherit_fds` row copy. Nothing reads it yet; that is what makes
the rest reversible.

## Slice 4 — the mirror

The leaf syscalls (`dup`/`dup2`/`dup3`/`fcntl`/`close`/`lseek`/`fstat`) are
the first readers, but no descriptor can be read from the table before
`open` lands there — so slice 4 is the **mirror**: every file, pipe and
socket description `fd.rs` interns in `FILES` is inserted into the calling
process's registered table under the same fd number (`install`, `sys_dup`,
`dup_onto`), removed by `sys_close`, its cursor synced by `sys_lseek`, its
`O_NONBLOCK`/`FD_CLOEXEC` bits mirrored by `sys_fcntl` into the table's
`nonblock`/`cloexec` sets (`F_GETFD` reads the set back; **enforcement at
execve is still absent — the divergence stays pinned**).

The design rule that carried the slice: **the mirror owns no references.**
`FILES` stays the refcount authority; `clone_fd_refs` is never called on a
mirror; the pipe/socket refcounts move to the table only when slice 5/6 flips
authority, in one slice, explicitly.

The pipe hooks (`pipe_close_read`/`pipe_close_write`/`pipe_clone_ref`) became
real in `exec_runtime.rs` — wired to `crate::pipe`. The old warning that this
mapping would be "actively wrong" described a world where no `SharedFdTable`
here ever carried a pipe id; in the mirror world the variant's payload *is* a
`crate::pipe::PipeId`, because `crate::pipe` is the only pipe allocator this
target has.

### The bug the mirror's first boot charged (double-bump)

First Firecracker boot: **510/2** — `redirect: yes | head -n 1` fails, plus a
leaked pipe buffer. Cause: slice 3's fork path still called
`clone_deep_for_fork`, whose `clone_fd_refs` bumps pipe refs — while
`inherit_fds` had already bumped each one. Double-bumped at fork, released
once at close: the write end never reached zero, `yes` blocked forever.

Fix: `fd::fork_table_mirror` — the fork path clones the table **without**
ref bumps, because the mirror owns none. The function's header records that
it becomes `clone_deep_for_fork` on the day authority flips.

### The wedge (the metal found what Firecracker forgave)

First metal workload after the mirror: a 30-grandchild fork loop under sshd,
and the box died — a full framebuffer of `[BKL] stuck: owner=N waiter=M
tag=511` interleaved with `[TLB] stuck: 1 peer(s) unlocked`. No ping; power
cycle required.

Cause, found by reading `akuma-exec` with the failure in hand:
**`impl Drop for SharedFdTable` runs `close_all()`, which fires the
`ExecRuntime` close hooks per entry.** Every entry in the table being a
mirror, a dying child's `Process::drop` executed real `pipe_close_read`/
`pipe_close_write` on pipes it did not own — including the shell's own stdio
bridge onto `sshd`. The Firecracker suite passed because its pipes were
already EOF'd when children exited; sshd's topology is not. (The `File` arm
would have reached the `not_wired!` `flock_release` for any child dying with
an open file mirror.)

Fix: `fd::clear_table_mirror()`, called from `run_process`'s exit path
immediately after `close_owned_by` — the legacy world has just accounted for
every reference, so the hooks have nothing left to do, and the table is
empty before the `Arc` can drop. **The general rule this pins:** while
`FILES` owns the refcounts, a `SharedFdTable` on this target must never
reach a `Drop` with mirrors still in it. If slice 5/6 misses this, the drop
is the thing that says so — the hard way.

### Verification status

- Firecracker SMP=4: 512/0 after the double-bump fix.
- The metal wedge fix (**clear_table_mirror**): verified 2026-09-09, same
  day — Firecracker 512/0, metal suite 516/0, and the exact workload that
  wedged the previous boot (pipeline, subshell, `sh -c`, file redirect, the
  30-grandchild loop) passing with the box alive and `Cached:` back at
  1611 kB after.
- Local QEMU/TCG was skipped for these slices by explicit instruction; the
  suite it would run is the same one Firecracker runs.

## An observation that is *not* C2's

The metal accrues ~10 `[BKL] stuck: tag=511` lines per ssh command, on every
binary tested — including a pre-slice-1 backup booted specifically for the
A/B (160 baseline, +9–10 per command, identical rate). **Pre-existing**, not
caused by any C2 slice. The aarch64 kernel shows similar numbers, apparently
not from forking. `tag=511` is the profiler-off sentinel: this target never
wires the BKL holder tag, so the holds are unattributed. Worth its own
investigation; the wedge above is a reminder that long unattributed BKL holds
+ the TLB shootdown's "sender holds the BKL" wait is a live deadlock
combination (`AKUMA_AMD64_TLB_SHOOTDOWN.md` carries that invariant).

## Where this leaves the plan

- Slices 1–5 done. The whole-file cache is deleted; the OOM class it caused
  cannot recur, and the original reproducer runs to completion with a flat
  heap. `Entry.data` survives only as the storage for synthetic `/proc`
  renders.
- Slice 6 (pipes into the table, `Spawn`'s four fields, the `wait4` decision)
  and slice 7 (`/proc`) remain. The mirror rule and both helper obligations
  (`fork_table_mirror`, `clear_table_mirror`) carry forward unchanged until
  the refcount authority moves to the table.

## Slice 5 — `open`/`read`/`write`/`pread64`: the cache dies (2026-09-09, same day)

The discovery that made this slice cheap: **the partial-I/O surface already
existed**. `akuma-vfs-glue` had `read_at` *and* `write_at` (path, offset,
bounded buffer), ext2 implements both as real block-level partial I/O, and
`amd64/src/fs.rs` re-exports `read_at` — nobody had ever wired a descriptor
to them. Slice 5 is pure rewiring in `fd.rs`:

- **`openat`** hands out descriptors with an **empty** buffer. Existence is a
  *one-byte* `read_at` probe (see the probe bug below), `O_APPEND`'s starting
  cursor is one `metadata`, and `O_TRUNC` truncates **once, at open, through
  the VFS** — the old design truncated implicitly at close by persisting an
  empty buffer, which also meant `O_CREAT` without `O_TRUNC` destroyed the
  file; that bug dies with the cache.
- **`read`/`pread64`/`file_bytes_at`** (the mmap backing) split on
  `entry_is_synthetic`: real files answer through `fs::read_at` at the cursor
  in `MAX_IO`-bounded chunks, disk I/O outside the `FILES` lock (the rule
  `release` already stated); synthetic `/proc` views keep their cached render,
  because there is no inode behind the path to read from.
- **`sys_write_file`** writes through `write_at` at the cursor, chunk by
  chunk; `ENOSPC`/`EROFS`/`EIO` reach ring 3 via `fs_err_errno`.
- **`lseek`'s `SEEK_END`** and **`fstat`'s size** take the size from
  `metadata` for real files, from the cache for synthetic ones.
- **`release`'s persist arm** now carries a `!data.is_empty()` guard: a real
  close is a no-op (POSIX: close does not truncate), and persisting an empty
  buffer would have *truncated* every read-only-closed file.

The real/synthetic discriminator is `data.is_empty()`, sound because every
synthetic installer renders non-empty contents; if a future view can render
empty, give it a flag instead.

### The probe bug (first FC boot, 519/5)

The existence probe was originally **zero-length** — and ext2's `read_at`
returns `Ok` for a zero-length read *before resolving the path*, so every
missing file opened successfully, shifted every fd number by one, and failed
the suite's capacity/row tests as collateral. Existence checks and short-read
semantics are different questions; a probe must not inherit the byte path's
short-circuit. One-byte buffer; fixed; **512/0**.

### Verification

- Firecracker SMP=4 boot suite: **512/0** (twice: once failing with the probe
  bug, once green after).
- Bare metal: all self-tests passed, then the real ring-3 workload —
  redirects (`>` and `>>`), `wc -c` (`SEEK_END` through `metadata`), sizes —
  all correct.
- **The witness, final form:** the original kill-the-machine ladder
  (`cat /bin/akuma` × 120 into one held fd = 168 MB target) ran to **rc=0,
  full 168 921 600 bytes — past the old 134 MB `ENOMEM` ceiling — with
  `Cached:` flat at ~1.7 MB the whole time**. The heap cost of a write is now
  one 64 KiB chunk, not three times the file.
- `amd64_mem_trials.py --smp 4`: **8/10, 0 unexpected, on both the TCG and
  the Firecracker arm** — `mmapsum` (pread loops), `mmap_file` (file-backed
  mappings served by the VFS-backed `file_bytes_at`) and `cowstale` all green
  against the cache-less world. The two "known" fails are the documented
  no-signal-delivery gaps, untouched by this work.
- `mem_suite.py` over ssh to the **metal** was attempted and abandoned as a
  harness limitation, not a kernel one: its probe upload is a bulk
  stdin-over-ssh transfer, and the metal's pre-existing BKL stalls throttle
  that below the harness's timeouts. The FC arm runs the same ten probes.
- AArch64 clippy and host tests: clean.

### What slice 5 leaves for slices 6–7

- `dup(0)`/`dup(1)` on *unbound* stdio is still `EBADF` — the data path now
  routes through the table's *File* entries for files, but Stdin/Stdout/Stderr
  still route by number; lifting that is natural the day `sys_read`/`sys_write`
  consult the table for stdio variants.
- The mirror still owns no refs (`fork_table_mirror` unchanged);
  `clear_table_mirror` still runs at exit and is still required — `close_all`
  would fire pipe hooks on mirrors. Both flip together when the refcount
  authority moves.
- `O_APPEND` re-seek-per-write and cloexec enforcement at execve remain pinned
  divergences, now smaller (writes are no longer whole-file clobbers).

## Background

- `proposals/NEXT_AGENT_AMD64_C2_FD.md` — the plan, its baselines, and the
  slice order followed here.
- `proposals/AMD64_FD_WHOLE_FILE_HEAP.md` — the crash, its ladder, and why
  `free` is blind to it.
- `docs/archive/AKUMA_AMD64_TLB_SHOOTDOWN.md` — the shootdown, and the BKL
  invariant the fault path carries (visible in the wedge's console).
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE1_PROCESS.md` — the registration
  pattern slice 3 follows.
- `crates/akuma-exec/src/process/fd.rs` — `SharedFdTable`, `close_all`, and
  the `Drop` that makes the mirror rule load-bearing.
