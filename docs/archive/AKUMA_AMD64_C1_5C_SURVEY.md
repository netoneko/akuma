# C1 step 5c, surveyed: `fork` cannot fold, and why

**Date:** 2026-09-09
**Status:** survey, plus one fix that fell out of it and landed.
**Predecessor:** `AKUMA_AMD64_STEP5B_SLICE4_PROCS.md` — 5b is finished; this is
the step after it.

---

## What 5c was supposed to be

`AKUMA_SELF_HOSTING_AMD64.md`'s C1 box says
`Spawn/PROCS → akuma-exec: real fork/exec/lifecycle/reclaim`. 5b did `PROCS`
and lifecycle. 5c is the four calls themselves — `fork`, `execve`, `wait4`,
`clone` — onto `akuma-exec`'s `children.rs` / `spawn.rs` / `exec.rs`.

Slice 4 is what made that *readable*: those four now consult one process table
instead of two. It does not make it *writable*, and the reason is worth writing
down before someone spends a session discovering it.

## `fork_process` cannot be called from this target

Its tail is not bookkeeping. After the CoW share pass it builds an **AArch64
`UserContext`** — `x0 = 0` for the child's return value, `spsr = 0` for "EL0t,
interrupts enabled", `ttbr0` re-pointed at the child's own space — and hands it
to `spawn_child_thread_and_publish`, which starts the child through
`akuma-threading`'s user-thread machinery. That machinery enters userspace by
`eret`ing from that register file (`akuma-el0-entry`).

amd64 has no `eret`. A `fork` child here is a `sched::spawn_in_space_unpublished`
task whose first ring-3 entry is `enter_user_mode_forked`, reading the x86
register file out of its own per-task `UserCtx` — `saved_regs`, `fs_base`,
`gs_base`, seeded by `sched::seed_forked_task`. The 31 `x` registers of
`UserContext` have no meaning on this machine, and `spsr`/`ttbr0` have no
counterpart.

So folding `fork` means giving `spawn_child_thread_and_publish` an architecture
seam for *"start this process's first entry to ring 3"* — an x86 arm of the EL0
entry path. That is not a mechanical fold, it is the entry seam, and it is a
larger piece than any slice of 5b. **It should be scoped as its own step, not
started as "5c".**

`execve` is closer but not free: `replace_image` allocates and maps a
**ProcessInfo page** (mandatory, `?` on OOM) and pushes **lazy regions**. This
target registers `process_info_phys: 0` as a stated decision — the page is never
read here, and mapping one would leak 4 KiB per process past the ledger — and
demand-pages from `mmap_regions` rather than a lazy map. Adopting
`replace_image` means adopting both, which is a capability change wearing a
refactor's clothes.

`wait4` **does** have a fold target, and this survey's first draft said it did
not — the correction is worth keeping because the wrong version makes the work
look bigger than it is. It is `akuma_syscalls_glue::proc::sys_wait4`
(~165 lines, dispatched at `lib.rs:844` on `nr::WAIT4`), and glue is precisely
what C1 folds *into*. Nothing needs writing.

What blocks it is what that function stands on. Every primitive it uses —
`is_child_of_group`, `get_child_channel`, `has_children`, `reap_child_channel`
— reads one structure, `children.rs`'s `CHILD_CHANNELS`
(`BTreeMap<Pid, (Arc<ProcessChannel>, Pid)>`), and the blocking wait is
`ch.add_poller(tid)` + `schedule_blocking`. **amd64 never writes that map**: it
registers `channel: None` (a stated slice-1 decision — this target's stdio
bridge is `crate::pipe`) and calls `register_child_channel` nowhere. Folded
today, glue's `wait4` would answer `ECHILD` to every wait on this machine.

And the two kernels answer the question from different sources, which is the
part that makes this more than wiring: glue asks `CHILD_CHANNELS` whether a
child exists and a `ProcessChannel` whether it has exited, while amd64's
`sys_waitpid` reads `parent_pid` / `exited` / `exit_code` off the registered
`Process` itself (5b slices 2 and 4). Closing the gap is either "amd64
populates `CHILD_CHANNELS`" or "glue's `wait4` learns to read `Process::exited`"
— a design decision, not a move. Same C2 wall as `Spawn`'s four stdio fields,
reached from the other side.

## What did land: `execve` kills its siblings

The survey's one actionable finding, and it is a real divergence rather than a
missing fold. `replace_image` opens with `kill_exec_siblings`; this target's
`execve` did nothing, so a `CLONE_VM` sibling kept running in the address space
`execve` had just replaced and freed.

It was not a use-after-free — `free_or_defer_as_frames` parks the frames while
another core's `CR3` stands on that L0 — which is exactly why it had never been
noticed: the sibling runs the *old program* inside a process that has become a
different one, and the parked frames come back only when it eventually leaves.
POSIX destroys those threads; so does the AArch64 kernel.

`sys_execve` now drains the group before the swap, using the primitives this
target already had: `thread::drain` sets the group-exit flag, wakes every
sibling so a parked `FUTEX_WAIT` does not have to wait out the scheduler
backstop, and spins (bounded) until none is live. It runs after the new image is
built and before anything is swapped, so a failure there still leaves the
caller's own image untouched, and the existing `clear_group_exiting` after the
swap is what lets the new program's first thread run.

**One case is carried, not fixed.** `THREADS` holds non-main threads, so
`live_count` includes a *non-leader* caller: draining from one would spin the
full budget waiting for the thread doing the draining, then print
`DRAIN INCOMPLETE`. POSIX says such a caller becomes the group leader and the
others die. Leader transfer is something this target's thread model does not
have, so a non-leader `execve` with live siblings keeps the old behaviour and
says so on the console. That is the honest shape until the thread model grows
one.

## Verified

| rig | result |
|---|---|
| QEMU/TCG `SMP=1` | **515 / 0** |
| QEMU/TCG `SMP=4` | **524 / 0** |
| Firecracker (the box, KVM) `SMP=4` | **511 / 0** |
| **bare metal** `SMP=4` | **515 / 0** |
| ring-3, local QEMU, 40 sessions | 40/40, `free` unmoved, `grandfork` ALL PASS |
| ring-3, **bare metal**, 30 sessions | 30/30, `free` unmoved at 2298476, `ps` steady at 5 |
| `thread:` boot checks (the `CLONE_VM` probe) | all OK, "no thread outlived the process" |

The metal arm matters more than usual here: the change is on `execve`, which the
multiboot2 boot path reaches only through a real ring-3 session.

## What the roadmap should say now

C1's remaining work is **not** "5c, a fold". It is:

1. **The ring-3 entry seam** — an x86 arm for "enter userspace with this
   process's first context", which is what unblocks `fork`, and after it
   `clone`. Sized like 5b, not like a slice of it.
2. **C2** — `fd.rs` into glue, which is what unblocks `Spawn`'s four stdio
   fields and, with them, `wait4`.

Neither is blocked on the other, and both are blocked on neither of 5b's
outputs — 5b is genuinely finished.

## Background

- `crates/akuma-exec/src/process/mod.rs` — `fork_process`'s tail,
  `spawn_child_thread_and_publish`.
- `crates/akuma-exec/src/process/image.rs` — `replace_image_from`,
  `kill_exec_siblings`.
- `amd64/src/thread.rs` — `drain`, `wake_group`, `current_is_main`, and the
  lifetime rule the drain enforces.
