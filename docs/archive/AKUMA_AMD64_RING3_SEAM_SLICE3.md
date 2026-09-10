# amd64 ring-3 entry seam, slice 3: `CHILD_CHANNELS`, and `wait4` becomes shared code

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row.
**Slice:** 3 of 4 — but **not the slice the prompt scoped**, and § 1 is why.

Every child this kernel creates now carries an exit `ProcessChannel`, registered
in `CHILD_CHANNELS` exactly as the shared `spawn_child_thread_and_publish` does
on the other kernel, and `wait4` is served by `akuma-syscalls-glue`. This
target's own `wait4` loop, its global waiter bitmap and its private wake path
are gone.

**One file changed** (`amd64/src/usermode.rs`, +184/−72). The AArch64 kernel is
not in this diff at all.

## 1. The scoping correction: what actually blocks `fork_process`

`proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` § 6 scoped slice 3 as
"`sys_fork` calls `fork_process` — the actual fold. Expect the `CHILD_CHANNELS`
decision from § 5 to bite here and nowhere earlier."

That is measured from a **compile-level** fact (slice 1: `fork_process` compiles
for `x86_64-unknown-none`) and it is wrong about what bites first.
`CHILD_CHANNELS` is the *last* thing in the way. Ahead of it are three arch
seams, and one of them is silent:

| in the way | state on x86_64 |
|---|---|
| `akuma_threading::get_saved_user_context` | **stub returning `None`.** The read mirror of the `update_thread_context` slice 2 gave an x86 arm. `fork_process` calls it at step 6 and fails the syscall on `None`, so this is a loud, one-function fix: an `X86ArchHooks::read_user_context` over `machines()[slot].uctx`, the same table slice 2's writer edits. |
| `ThreadPool::spawn_user_closure_initializing` | **stub returning `Err`.** `spawn_child_thread_and_publish` spawns through it. Also loud. Needs more than a forward: an amd64 process task carries a `space_root` and a `proc_slot` that shared code has no concept of. |
| **`fork_process`'s memory pass** | **compiles, and is wrong.** This is the one that matters. |

### 1.1 The memory pass is an AArch64 page-table walker with no `cfg`

`fork_process` copies or CoW-shares the parent through
`translate_user_va`, `collect_mapped_pages_with_flags_into`,
`for_each_mapped_user_pte` and `demote_range_to_ro` — a family in `akuma-mmu`
that takes a raw `*const u64` L0 pointer and walks it with **AArch64 descriptor
semantics**: `flags::VALID`, `flags::TABLE`, the ARM block/table distinction,
and `AP_RO_ALL`/`UXN`/`PXN` for permissions. None of it is `#[cfg]`-gated, so it
compiles for x86_64 and would be handed a PML4.

The bits do not merely disagree, they **near-miss**: ARM's `VALID` is bit 0 and
so is x86's `Present`, and ARM's `TABLE` is bit 1 where x86 has `R/W`. A walk
would descend or stop according to whether the parent's pages happen to be
writable, and produce a child whose address space is garbage without erroring
anywhere.

This is not a divergence to fold away. amd64 already does the same job
correctly, through `UserAddressSpace::rewrite_leaves_in_range` — the leaf
iterator that **has both arms** (`x86_walk_leaves` on one side, the ARM walk on
the other) — in `Image::fork_of`: one hold of the parent's address space, CoW
share, demote in place, `shared_anon` by identity, region extents carried.

So folding `fork_process` means giving its memory pass an architecture seam, and
there are two shapes for that:

* **Rewrite `cow_share_and_demote_range` onto the leaf iterator.** One
  implementation, both kernels. It touches the AArch64 fork hot path — the one
  path the self-hosting campaign leans on hardest — so it is a large,
  regression-sensitive change to shared code that currently works.
* **Make the pass a registered hook**, the way slice 2 made ring-3 entry one.
  AArch64 registers today's sequence, amd64 registers today's `Image::fork_of`
  walk, and everything else in `fork_process` — identity, the `ProcessInfo`
  page, the child context, the publish ordering, reaping — folds. Two
  lifecycles, one stated difference.

The second is the same argument § 4 of the prompt made for the entry seam, and
it won there for the same reason. **Neither is this slice.**

### 1.2 What was done instead, and why it is the right half first

The user's call was "amd64 should follow arm64 practices", and the practice at
issue — `CHILD_CHANNELS` as the parent/child link and the exit-status authority
— **does not need `fork_process` at all**. The child's *memory* is architecture
work; the child's *lifecycle* is not. So this slice takes the lifecycle,
verifiably, and leaves the memory pass to be seamed on its own baseline.

That also answers § 5's unmeasured question ("does `Spawn` + `wait4` before
slice 3 make slice 3 smaller?"): with `CHILD_CHANNELS` as the source of truth
they were never two steps.

## 2. What landed

### 2.1 Every child gets an exit channel

`register_exec_process` — the one place this target builds a registered
`Process`, for `fork`, `spawn` and the boot self-tests alike — now ends with the
two registrations the shared spawn path performs:

```rust
let exit_channel = Arc::new(ProcessChannel::new());
register_channel(task_slot, exit_channel.clone());   // per-thread registry
register_child_channel(pid, exit_channel, ppid);     // CHILD_CHANNELS
```

`Process::channel` stays `None`: that field is a process's **I/O** channel, and
stdio here is bound descriptors over `crate::pipe`. This one carries an exit
status and nothing else — the same distinction the shared path's own comment
draws.

The per-thread registration is not decoration. Signal delivery and
`should_interrupt_blocking_syscall` both resolve through `get_channel(tid)`, so
without it a `wait4` parked inside glue could not be interrupted.

### 2.2 The exit publishes through the shared path

`spawn_record_exit` ends in `publish_child_exit(dying, status)`, which marks the
channel exited — waking the pollers registered on **that child** — and then
raises SIGCHLD on the parent, in that order, because a shell's SIGCHLD handler
re-polls with `WNOHANG` immediately and must find the zombie. It publishes at
most once per death.

**SIGCHLD on this target is new.** Nothing raised one before; the exit was a
number in a table. Section 4.2's `wait` finding is where that would show up
first, and it does not (both arms behave identically).

The task's per-thread channel entry is removed at the end of `run_process`,
mirroring the AArch64 exit epilogue's `remove_channel(tid)` — and **after** the
publish, which is load-bearing: `publish_child_exit` raises SIGCHLD only when it
is the call that marked the channel exited, so setting the exit code first (as
the AArch64 site does, deliberately redundantly, on the same `Arc`) would make
the publish a no-op and the parent would never get the signal.

### 2.3 `wait4` is glue's

The `Syscall::Wait4` arm is `to_glue(Syscall::Wait4, …)`. What it replaced was a
hand-written loop over this file's own `sys_waitpid`, parking on `WAIT4_PARKED`
— a bitmap of every waiting task, woken wholesale on any child's exit, because
it carried no parent link and could not tell whose child had died. Three things
arrive that it could not do:

* **`ECHILD` for a non-child.** `is_child_of_group` resolves the recorded
  parent's *thread group*, so a multithreaded parent waiting from a non-leader
  thread gets the right answer — and Go's `os/exec` pidfd probe, which requires
  `ECHILD` from `waitid(P_PIDFD)` on itself, stops deadlocking against its own
  exit. The `SPAWN` row records a pid; it cannot answer this.
* **`EINTR`.** The old loop had no interrupt check at all: a `wait4` here was
  uninterruptible.
* **`rusage`.** Zeroed rather than ignored.

Measured effect on the boot suite: `sched: parks over the whole suite` **10 →
3**. Waiters park on the channel of the child they are waiting for instead of
cycling through a wake-everyone bitmap.

### 2.4 The `SPAWN` row is collected by a sweep, not a hook

glue's `wait4` reaps — `unregister_process` + `reap_child_channel` — and knows
nothing about this target's `SPAWN` table, nor should it: the two fields left in
a row are a scheduler task slot and a `crate::pipe` id, neither of which exists
on the other kernel.

A reap hook was the obvious alternative and is the wrong shape. The reap is not
the only way a row goes stale — a reparented orphan, a `wait4` that returned
`EFAULT` after unregistering, a future second waiter — and a hook must be called
from every one of them, correctly, forever. **"The process this row names is no
longer registered"** is checkable and true in all of them, so
`sweep_reaped_spawn_rows` asks the question. It runs after each `wait4` and
again when `fork`/`spawn` look for a free slot, which are the only two moments
the answer matters — the second is not optional, or the table fills with rows
for processes that no longer exist and `fork` reports `ENOMEM` on a machine with
gigabytes free.

It uses `pipe::close_write`, not `pipe::free`, for the reason
`Spawn::stdin_pipe`'s doc gives: `sshd` may still hold an open
`/proc/<pid>/fd/0` over that pipe.

### 2.5 Two reapers, one job — found by the heap column

There are now two reap paths: glue's `wait4`, and this file's `sys_waitpid`
(Akuma's private 303, which `sshd`'s bridge polls non-blockingly). Whichever
gets there first must do the whole job.

It did not, at first, and the witness was `amd64_ring3_check`'s `heap` column:
drift over 60 ssh sessions went from **+53 kB** (pre-change) to **+131 kB**,
about 200 bytes per child — an `Arc<ProcessChannel>` and its `CHILD_CHANNELS`
entry, left behind by whichever reaper had not been taught. Moving
`reap_child_channel` **inside `reap_exec_process`** (which has two callers, the
second being the easy-to-forget fallback for a child the process table knows and
no `SPAWN` row names) took it to **+36 kB**, below the pre-change figure.

The metric is noisy — four runs of the fixed kernel measured +36, +100, +132 and
+229 kB — so the number that settles it is not any single reading but that the
drift **does not scale with session count**: at `-n 120` it measured +100 kB,
not double the `-n 60` value. A per-child leak would double.

## 3. What this slice deliberately did not do

- **Call `fork_process`.** See § 1. `sys_fork` still builds the child itself,
  through `Image::fork_of`, which is the arch-correct walk.
- **Route the private `waitpid` (303) to glue.** It stays, because it answers
  `ECHILD` for an unknown pid where glue's counterpart answers `0`, and
  `sshd`'s bridge polls it in a loop — a `0` where an `ECHILD` was expected is a
  poll that never stops. It reaps the channel now, so the two paths agree.
- **Touch `sys_spawn`'s or `clone`'s child construction** beyond the shared
  registration in `register_exec_process`.

## 4. Verification

Every amd64 gate was run as an **A/B against the unchanged tree**, because the
baseline moved during the session (unrelated work landed +10 checks).

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=1` | 631/0 | **631/0**, `diff` of the check-name lists empty |
| QEMU/TCG `SMP=4` | 641/0 | **641/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | — | **609/0 · 619/0** (slice 2's 599/609 + the same +10) |
| bare metal `SMP=4`, `root=/dev/sda1` | 624+3 (slice 2) | **634 passed, 3 FAILED** — the same three pre-existing xHCI bulk-transfer failures |
| host tests | 1373 | **1373** |
| amd64 `--features no-tests` | OK | **OK** |
| clippy — amd64 with and without `no-tests` | clean | **clean** |

The AArch64 kernel needs no side-by-side this time: the diff is one file under
`amd64/`, which is not in that kernel's dependency graph at all.

The `SMP=1` row is the strongest of these — identical **check sets**, not just
identical counts. The only differences between the two logs are `t.note()`
measurement lines (TSC ticks, spin counts, elapsed µs), plus the parks/wakes
drop § 2.3 records.

### 4.1 The ring-3 gates

| gate | result |
|---|---|
| `amd64_ring3_check --smp 1 -n 40` | **40/40**, `free` unmoved, `ps` 5 → 5, heap +47 kB |
| `--smp 1 -n 60` | **60/60**, `free` unmoved, `ps` 5 → 5, heap +36 kB |
| `--smp 1 -n 120` | **120/120** on three runs of four; one run returned 119/120 (see below) |
| `grandfork`, all five rungs | **ALL PASS** on every run — including rung 3 (a forked child *blocking* on a grandchild) and rung 5 (`wait4(-1)` must `ECHILD` after the last reap), which are the two the shared implementation now serves |
| `lazybuf` / `openflags` | **8/8 · 20/20** |
| `busybox sh` pipeline over ssh | **OK** |
| `apk update` + `apk add file` + `file /bin/busybox` | **OK** (the `3 errors` are the documented `chown` residue) |
| the metal's own `wait4:` / `fork:` / `spawn:` / `busybox:` boot checks | **all `[OK]`** on real hardware |

`grandfork` is the wait4 probe this slice most needed and it already existed —
`amd64_ring3_check` runs it on every invocation.

**The one 119/120.** A single ssh session out of 120 did not return, on one run;
the same run's `free` was unmoved, `ps` steady and `grandfork` ALL PASS, and the
three subsequent `-n 120` runs returned 120/120. Recorded rather than explained:
at this scale the harness has a racy history, and one flake in four runs is not
evidence of a defect this slice introduced.

### 4.2 Pre-existing and **not** introduced: `wait` for a background job hangs

Found while probing this change, A/B'd, and present in **both** arms:

```
sleep 1 &  ; echo bg=$!; sleep 2; echo after   → works (bg=49, after=ok)
jobs                                           → works ([1]+ Running)
sleep 1; echo                                  → works  (a blocking wait4(pid))
sleep 1 & wait                                 → HANGS
sleep 1 & p=$!; wait $p                        → HANGS
true & wait                                    → HANGS
```

So it is **not** `wait4(-1)` versus `wait4(pid)` — the synchronous `sleep 1`
takes a blocking `wait4(pid)` and returns. It is *backgrounded* children
specifically, and it survives the move to the shared implementation unchanged,
which means the defect is not in either `wait4`. The next thing to look at is
what ash does differently for a background job — process groups (`setpgid`),
`WUNTRACED`, or its SIGCHLD-driven job table — and whether the exit ever reaches
the job the shell is waiting on.

Worth noting against this: SIGCHLD is newly raised on this target (§ 2.2), and
this is exactly the path where a newly-arriving signal could have changed
behaviour. It did not — both arms hang identically.

## Background

- `proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` — the step's prompt. § 6
  scoped this slice; § 1 above is the correction.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE2.md` — the ring-3 entry seam, and
  the `update_thread_context` x86 arm whose read mirror § 1 names as the next
  small piece.
- `docs/archive/AKUMA_AMD64_WAIT4_OWNERSHIP.md` — the `ppid` filter, and why
  `grandfork` has five rungs.
- `docs/archive/AKUMA_AMD64_COW.md` — amd64's own CoW fork, i.e. the walk § 1.1
  says already does the job correctly.
- `docs/archive/GO_FORKTEST_DEBUG.md` — the `is_child_of_group` /
  `waitid(P_PIDFD)` incident on the other kernel, and what `CHILD_CHANNELS`
  buys that a pid in a table cannot.
