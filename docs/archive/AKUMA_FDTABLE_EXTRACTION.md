# `akuma-fdtable`: the fd table's state machine leaves `akuma-exec`

**2026-10-21.** The fd-bookkeeping decision logic inside `akuma-exec`'s
`SharedFdTable` (`crates/akuma-exec/src/process/fd.rs`) became
`crates/akuma-fdtable`. Current behaviour and rules:
`docs/reference/subsystems/fdtable.md`.

## Why this logic, and why it was extracted test-first

Every fd bug in the archive is a property of one of a handful of decisions:
which number `open`/`dup` hands out, what flags a reused number inherits, what
a `dup` copies (and does not), what an exec cloexec sweep visits, and what a
fork copies. Each of those was previously open-coded inside lock-holding
methods in `akuma-exec`, testable only through the lock — and one of them had
already drifted for real: `dup`, `dup3` and `fcntl(F_DUPFD)` each carried
their own copy of the kernel-reference bump list until `clone_fd_refs`
replaced the copies (see that function's header in `fd.rs`).

The extraction follows the characterization-test-first rule: the semantics
were pinned **before** any code moved, against the old open-coded logic, so
the crate had an oracle to be diffed against rather than a description to be
re-derived.

## What moved

| old (`SharedFdTable` / `Process`, under a lock) | new (`akuma_fdtable::FdTable`) | routed? |
|---|---|---|
| `lowest_available_fd` (private walk) | `lowest_free_from` / `free_below(min, ceiling)` | yes — deleted; `alloc_fd_from` routes through the crate |
| `alloc_fd_from`'s insert + stale-flag clear | `alloc_fd_from` (clears the number's own stale bits before insert) | yes |
| `clone_deep_for_fork`'s copy loop | `fork_copy(keep)` — epoll stripping injected as the `keep` predicate; flag sets copied wholesale | yes |
| `close_cloexec_fds`' sweep | `sweep_cloexec` (present-only ascending; whole-set clear) | yes |
| dup/fcntl decision bits (glue's `sys_dup`/`sys_dup3`/`F_DUPFD` bodies) | `dup`, `dup_to` (`Err(SameFd)` for old==new), `dup_min` | pinned, not rewired — see below |
| glue's `sys_close_range` scan | `occupied_in_range` / `close_range` | pinned, not rewired |
| flag pokes scattered through `Process` | flag accessors + `with`/`update` borrows on the view | via the view where the wrapper acts |

The two "pinned, not rewired" rows are deliberate. Glue's dup and
`close_range` bodies are the *acting* half (user copies, per-variant kernel
releases); their decision content is now expressed — and differentially
tested — in the crate, but the bodies were left untouched, because this
change's contract was "glue and `amd64/src/fd.rs` do not change". Until they
are rewired, the crate's ops are the spec those bodies are held to by
`fdtable_characterization_tests`.

Deleted outright: `lowest_available_fd` (its only caller routes through the
crate now). `SharedFdTable::new` became `const fn` — the amd64 boot task
builds `KERNEL_TABLE` as a `static`, which the old `BTreeMap::new()` literal
satisfied and the wrapper now satisfies through `Names::new()`/`FlagSet::new()`.

## The shape: a borrowing view, effects returned not performed

The kernel keeps the acting half. `akuma-exec`'s `SharedFdTable` still owns
the three spinlocks (`table`, `cloexec`, `nonblock` — locked separately, so
the crate takes all three as one borrowed `FdTable<'t, D>` view built by the
`fd_view!` macro inside one `with_irqs_disabled` hold) and still performs
every effect: kernel-reference bumps via `clone_fd_refs`, releases via
`release_fd_entry`, user copies in glue. Ops that would allocate or release
(`dup_to`'s displaced entry, `fork_copy`'s child parts, `dup`'s copy) return
the payload; the caller acts on it after the lock is down.

`Names<D>` is generic over the payload; only `akuma-exec` instantiates it
with `FileDescriptor` today. `SharedFdTable` gained a `Deref` to its fields so
the pre-existing direct field borrows outside it (`akuma-syscalls-glue/src/fs.rs`,
`akuma-vfs-glue`'s proc.rs, `amd64/src/usermode.rs`, `src/process_tests.rs`)
compile unchanged — **`amd64/src/fd.rs`'s only change is the `KERNEL_TABLE`
literal's three constructor names**, forced by the types moving.

## Allocation analysis

Three collection nodes per occupied fd (`BTreeMap` + two `BTreeSet`s), all
`const`-constructible empty — the `KERNEL_TABLE` static needs that — and
sparse, which fork-heavy shells want. The rejected alternatives (a 4 KiB
fixed array per process, `akuma-slot-table`'s lock-free slots, a sorted
small-vec) and the reasoning are in the crate's module doc. No op allocates:
the `Forked`/`Dup`/`DupTo` returns hand back values the caller already owns.

## Measurements

- `akuma-fdtable`: 11 host tests pass, including a 2000-operation LCG-driven
  differential against a naive model (every op interleaving checked for fd
  choice, flag state and returned effects).
- `akuma-exec`: 104 host tests pass — the 97 baseline plus 7 new
  `fdtable_characterization_tests` written against the **pre-extraction**
  logic and kept as the wrapper's pin. The 2 pre-existing
  `ryzen_2026_10_16_zombie_tid_tests` failures (page-alignment panic, not
  fd-related) are unchanged.
- `scripts/check_unsafe_budget.py`: `akuma-fdtable` is a new crate under
  `forbid` with **0 unsafe sites**, as the gate requires. `--update` was
  deliberately **not** run in this change: amd64/src sits at 81 vs budget 79
  from prior work on this branch, and the update would have absorbed those
  unrelated sites into a diff that has nothing to do with them.

## The dup fix (recommendation, deliberately not done)

POSIX says `dup` shares the open file description: two descriptors, one
offset. Here `dup` copies the payload by value (`FileDescriptor` is `Clone`),
so two descriptors on one file carry independent cursors, and `nonblock` is
keyed by fd *number*, so the copy loses the flag. Both are pinned at three
layers (crate tests, `akuma-exec` characterization tests, amd64 boot suite)
and must not be "fixed" casually.

The fix, if it is ever wanted: move the payload behind one refcounted cell
per description (`Dup { fd, payload }` would bump a description count instead
of re-bumping every kernel object), make the description count *be* the
kernel ref count, and `dup` becomes allocation-free — a count increment under
the table lock. That is a cross-cutting ABI change, not a crate refactor:

- it rewrites `clone_fd_refs`/`release_fd_entry` and both kernels' dup bodies
  (`akuma-syscalls-glue/src/fs.rs`, `amd64/src/fd.rs`);
- it changes `SCM_RIGHTS` fd passing, which re-bumps per received copy today;
- it changes `fork_copy`, whose child entries currently each hold one bump;
- and the current behaviour is pinned by the boot suites, which could not be
  re-validated where this work happened (no QEMU/Lima access) — the
  characterization tests pin the wrapper, but a change of this size needs the
  live suites green on the *new* semantics first.

So: designed, written down, and left. The crate's doc comments point at this
section; `docs/reference/subsystems/fdtable.md` § "Pinned divergences" points
here too.

## Stale premises corrected alongside

- `AKUMA_FIRECRACKER_AMD64.md` §3.24.4 said the fd table was "still one
  table, not one per process"; the per-process table landed on amd64
  2026-09-06 (`AKUMA_AMD64_C2_SLICES_6_AND_7.md`, C2 step 4b). Dated
  correction added there.
- `POST_AMD64_CODE_AUDIT.md` §4.3 proposed extracting the fd table *onto
  `akuma-slot-table`* and called it "a spawn-ceiling" fix. The per-process
  table already existed; the extraction landed on `BTreeMap`-backed
  `Names<D>` and rejected the slot-table machinery. Dated correction added
  there.

## Verification

Run by the extracting agent (host, `x86_64-unknown-linux-musl`):

```bash
cargo test -p akuma-fdtable  --offline -j1 --target $HOST   # 11 passed
cargo test -p akuma-exec     --offline -j1 --target $HOST   # 104 passed, 2 pre-existing fail
cargo clippy -p akuma-fdtable --offline -j1 --target $HOST -- -D warnings  # clean
cargo check -p akuma-amd64 --offline -j1 --target x86_64-unknown-none      # green
python3 scripts/check_unsafe_budget.py   # fdtable NEW-FORBID at 0; amd64/src +2 is pre-branch
```

**Verification the user must run** (no QEMU/Lima in the extraction
environment):

```bash
cargo check --offline          # AArch64 default target
# Lima boot: expect 300 passed / 1 known fail (test_epoll_socket_waker)
# ryzen rehearsal:
DISK=nvme KERNEL=<tests build built on ryzen> sh overlays/ryzen/qemu.sh 0 300 std
#   expect 804 passed, 0 failed
```

## Background

- `docs/reference/subsystems/fdtable.md` — the current-state reference.
- `docs/archive/AKUMA_AMD64_C2_SLICES_6_AND_7.md` — the per-process table
  arrived on amd64 (C2 step 4b, 2026-09-06).
- `docs/archive/NCA_FD_NONBLOCK_TOCTOU.md` — why `alloc_fd_from` clears the
  reused number's flags.
- `docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md` — why every
  refcounted variant has exactly one bump arm.
- `docs/archive/CONCURRENT_WRITE_POSITION_RACE.md` — the
  `reserve_write_pos` lock-hold sequence (stayed in the wrapper; it is
  payload mutation, not fd-number bookkeeping).
