# The fd table (`akuma-fdtable`)

**Stability: B** — extracted 2026-10-21 from `akuma-exec`'s `SharedFdTable`
(`docs/archive/AKUMA_FDTABLE_EXTRACTION.md`); behaviour-preserving by
characterization test.

`crates/akuma-fdtable` is pure, `#![forbid(unsafe_code)]`, has **no
dependencies** and nothing in it locks. The kernel keeps the acting half:
`akuma-exec`'s `SharedFdTable` (the three spinlocks and the reference
effects), `akuma-syscalls-glue/src/fs.rs` (the dup/fcntl/close_range syscall
bodies, user copies, kernel-object release), `amd64/src/fd.rs` (the `MAX_FDS`
ceiling, the boot task's `KERNEL_TABLE`).

| item | what | who calls it |
|---|---|---|
| `Names<D>` | fd number → description, as a `BTreeMap` newtype that derefs to the map | everyone with a read-side lock (`/proc/<pid>/fd`, spawn stdio checks, `sys_close_range`'s scan) |
| `FlagSet` | fd numbers carrying `FD_CLOEXEC` or the per-number nonblock bit, as a `BTreeSet` newtype | the state machine; kernel readers via `Deref` |
| `FdTable<'t, D>` | **the state machine**: lowest-free allocation, `dup`/`dup_to`/`dup_min`, `close`/`close_range`, `sweep_cloexec`, `fork_copy` (epoll stripping as a stated `keep` policy), flag accessors, `with`/`update` borrows | `SharedFdTable`'s methods, inside one IRQ-masked critical section |
| `Dup`/`DupTo`/`Forked` | the effects, returned not performed: a copied payload to take one kernel reference on, a displaced payload to release, a child's parts whose payloads each need one bump | `clone_fd_refs` / `release_fd_entry` at the call sites |

## Who owns each fd property

| property | owner | keyed by |
|---|---|---|
| fd number → description | `Names<D>` (the `table` spinlock) | fd number |
| `FD_CLOEXEC` | `FlagSet` (the `cloexec` spinlock) | **fd number** (pinned divergence) |
| `O_NONBLOCK` | `FlagSet` (the `nonblock` spinlock) | **fd number** (pinned divergence) |
| file offset / `O_APPEND` / dir cache | the payload `D` itself (`KernelFile`) | the description |
| the reference a pipe/socket/eventfd/pty holds | the kernel object's own table; one per table entry | bumped per copy (`clone_fd_refs`), released per removal (`release_fd_entry`) |

The kernel-reference rule is one sentence: **every entry in a table is backed
by one reference, taken at the copy that installed it and released at the
close that removed it.** The state machine never performs either half; its ops
hand back the payload to bump or release, and `akuma-exec`/glue perform them
after dropping the locks.

## Pinned divergences (do not "fix" casually)

Both are glue's adopted `dup` model, pinned at three layers: the crate's
tests, `akuma-exec`'s `fdtable_characterization_tests`, and the amd64 boot
suite (`amd64/src/fd.rs`: "a dup's cursor is independent", "a dup loses
nonblock"):

1. **`dup` copies the payload by value** — two descriptors on one file carry
   independent cursors. POSIX shares the open file description. The design
   that would fix it, and the reason it stayed a recommendation, is
   `docs/archive/AKUMA_FDTABLE_EXTRACTION.md` § "The dup fix".
2. **`nonblock` is keyed by fd number** — `dup` of a non-blocking descriptor
   loses the flag (`dup_to`/`sys_dup3` copies it; `dup`/`sys_dup` does not),
   and an `alloc_fd_from` always clears both bits for the number it hands
   out, so a closed fd's stale bit can never survive a reuse
   (`docs/archive/NCA_FD_NONBLOCK_TOCTOU.md`).

## Rules

- **The state machine is the only writer.** Outside `akuma-fdtable`, no code
  computes a lowest-free fd, sweeps cloexec, or copies a table for fork; the
  read-side `Deref` borrows (`get`/`contains_key`/`range`/`keys`) are the only
  sanctioned direct pokes, always under a lock held by the caller.
- **The ceiling is policy.** `FdTable` is ceiling-free; `MAX_FDS = 1024` is
  enforced at `amd64/src/fd.rs` (`install`, `sys_dup2`) and mirrored by
  `free_below(min, ceiling)` for callers that want the bounded question
  answered where the crate can see it.
- **Allocation:** three collection nodes per occupied fd, `const`-constructible
  empty (the `KERNEL_TABLE` static needs that), ascending iteration (the sweep
  and `pop_lowest` orders are pinned against it). The rejected alternatives —
  a 4 KiB-per-process fixed array, `akuma-slot-table`'s lock-free slots, a
  sorted small-vec — and the reasoning are in the crate's module doc.

## Background

- `docs/archive/AKUMA_FDTABLE_EXTRACTION.md` — the extraction, what it
  measured, and the dup-fix recommendation.
- `docs/archive/AKUMA_AMD64_C2_SLICES_6_AND_7.md` — the per-process table
  arrived on amd64 2026-09-06 (C2 step 4b); before that the table was one
  flat array for the whole machine.
- `docs/archive/NCA_FD_NONBLOCK_TOCTOU.md` — why `alloc_fd_from` clears flags.
- `docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md` — why every
  refcounted variant has exactly one bump arm (`clone_fd_refs`).
