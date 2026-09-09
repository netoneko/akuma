# amd64 C1 step 4b, the flip: the registered `SharedFdTable` is the only descriptor table

**Date:** 2026-09-09
**Status:** landed. QEMU/TCG `SMP=1` **550/0**, `SMP=4` **560/0**;
Firecracker/KVM `SMP=1` **537/0**, `SMP=4` **547/0** (first run with the box's
Firecracker arm live — the 4b baselines were set with it skipped); ring-3
`-n 30` **OK** (`free` unmoved, `grandfork` ALL PASS); memory probes **8/10,
0 unexpected on both transports**; clippy (`akuma-amd64`) clean.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `docs/archive/AKUMA_AMD64_4B_PREREQUISITES.md` — slices A, B
and C; the open decisions it recorded are all closed below.
**Next:** step 2, the arms — begun: the five path-only `*at` syscalls folded
in `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH1.md`; the rest per
`proposals/NEXT_AGENT_AMD64_4B_VFS_FOLD.md` step 2.

`FDS`, `FILES`, `Entry`, `FileIdx`, `KERNEL_ROW`, `fork_table_mirror`,
`inherit_fds`, `close_owned_by` and `clear_table_mirror` are deleted. Every
descriptor a task owns lives in its registered `SharedFdTable`, and every
`PipeRead`/`PipeWrite`/`Socket` entry in one is backed by one pipe-end/socket
reference: a copy bumps (`clone_refs`, the local exhaustive-match twin of
`clone_fd_refs`), a removal releases (`release_desc` → `pipe::close_*` /
`sock::close`), and `close_all` — the sweep `SharedFdTable::drop` always ran —
is the exit teardown.

The reference a pipe's `alloc` starts each end with is **consumed by the first
insert** (`sys_pipe2`, `bind_stdio`'s fd 0 and fd 1) and **bumped for every
later one** (`dup`, `dup2/3`, `fcntl(F_DUPFD)`, `fork` via
`clone_deep_for_fork`, `bind_stdio`'s fd 2, `alloc_pipe_fd`, whose two callers
are both always second names).

---

## The three decisions, taken

**1. `Entry::data` — the synthetic `/proc` render: re-rendered per `read(2)`.**
Not a side table (the second structure the flip exists to remove) and not a
`KernelFile` field (a shared-type extension for one architecture's sake).
Re-rendering is what the mounted `ProcFilesystem` — the fold destination, and
what glue serves `/proc` through on AArch64 — already does, so the target
converges on the tree's behaviour. The cost is the `seq_file` snapshot: two
reads of one `/proc` file can see two renders. A synthetic **directory** still
snapshots into `KernelFile::dir_cache`, unchanged. The discriminator is the
path prefix (`proc_rest_of`), which is sound because `sys_openat` intercepts
every `/proc` open — and which exposed decision-forcing bug §2 below.

**2. `nonblocking`: the table's `nonblock` set, keyed by fd number.** The
per-description `Entry::nonblocking` is gone; `F_SETFL` writes the set
`is_nonblocking` reads, and the old dual-bookkeeping (field + mirror, kept in
lockstep) collapses to one. Stated divergence: **`dup`ping a non-blocking
descriptor loses the flag** — glue's existing behaviour, now pinned by a
self-test.

**3. `dup` copies the `KernelFile` by value: independent cursors.** The flip
trades amd64's correct shared-cursor `dup` for glue's matching one, exactly as
the hand-off predicted. Fixing it honestly means an `Arc` inside
`FileDescriptor::File` in `akuma-exec-core` — a change to the shared type with
its own pass behind it. The AArch64 kernel self-hosts on this behaviour, which
is the evidence it is survivable. Pinned by the self-test that used to assert
the opposite, with a check that goes red if the divergence quietly changes.

---

## Two bugs the flip flushed out (both found by the first boot, both in code
the flip did not touch)

- **`register_exec_process`'s default table was `with_stdio()`.** Its
  `Stdin`/`Stdout`/`Stderr` entries were invisible while fd.rs consulted
  `FDS` rows; once the table was the authority, `is_bound(1)` became true for
  every test process and `write(1, …)` routed into `sys_write_file`, which
  refuses a non-`File` descriptor — every ring-3 test went silent. Default is
  `SharedFdTable::new()` now: on this target an unbound 0/1/2 *is* the
  console, and a table entry claiming fd 1 is a lie the new authority
  believes.
- **Synthetic `/proc` descriptors stored the bare rest as their path**
  (`1/statm`, not `/proc/1/statm`). Harmless while reads served cached bytes
  and never looked at the path; broken the moment `proc_rest_of` classified
  by prefix — every `/proc` file read became a failed `fs::read_at`. The
  installers store absolute paths now.

The shape is the one the prerequisites doc named: a stored value that was
never *read* before is read by the first caller that needs it, and whatever
the cache era papered over surfaces as a live failure.

---

## What changed at the call sites

- **`fork`** (`usermode.rs`): `inherit_fds` + `fork_table_mirror` collapse to
  one `parent.fds.clone_deep_for_fork()` handed to the registration. The bump
  and the copy are the same act now; the double-bump trap
  (`yes | head` blocking forever) is structurally impossible.
- **exit** (`run_process`): `close_owned_by` + `clear_table_mirror` become a
  `p.fds.close_all()` captured **before** `thread::drain` (which may retire
  the identity the lookup resolves through). The drop that follows later finds
  an empty table and is a no-op — idempotent by construction, no longer
  something every exit path had to defend against with a manual clear.
- **`sys_spawn`**: `bind_stdio(&child_fds, stdin, stdout)` operates on the
  child's table directly; the defensive `close_owned_by(slot)` row resets are
  gone (there is no per-slot table to reset); the `spawn_process_task` failure
  path runs `child_fds.close_all()` instead of a bare `table.clear()` that
  leaked the references.
- **Boot task**: `KERNEL_TABLE`, a static `SharedFdTable`, replaces the
  deleted `KERNEL_ROW` — `cur_table()` answers the registered process's table
  for a user task and this one otherwise, so the suite's `sys_openat` calls
  work unchanged.

## Verification

| gate | baseline | now |
|---|---|---|
| QEMU/TCG `SMP=1` | 546/0 | **550/0** (+4: pinned dup/nonblock divergences) |
| QEMU/TCG `SMP=4` | 556/0 | **560/0** |
| Firecracker/KVM `SMP=1` | not re-run at baseline | **537/0** |
| Firecracker/KVM `SMP=4` | not re-run at baseline | **547/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** — `grandfork` ALL PASS |
| `amd64_mem_trials --smp 4` (both arms) | 8/10, 0 unexpected (local) | **8/10, 0 unexpected, both arms** |
| clippy (`akuma-amd64`) | clean | clean |

Every check added is a *pin* on an adopted divergence rather than a pass
token: the dup cursor-independence check reads `lseek(b, 0, 1) == 0` where the
old one demanded `8`, and the nonblock check asserts the dup loses the flag
the original keeps.

## Background

- `docs/archive/AKUMA_AMD64_4B_PREREQUISITES.md` — slices A/B/C and the three
  decisions, stated before they were taken.
- `proposals/NEXT_AGENT_AMD64_4B_VFS_FOLD.md` — step 2, the arms; the
  suggested order and the pinned target divergences to restate.
- `crates/akuma-exec/src/process/fd.rs` — `clone_deep_for_fork`,
  `clone_fd_refs` and `close_all`, the crate machinery this flip points amd64
  at.
