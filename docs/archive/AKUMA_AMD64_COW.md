# Copy-on-write `fork` on amd64 (SMP=1)

**Date:** 2026-09-06
**Status:** working. 2000 `fork`s with zero memory drift and constant time.

## What was extracted, and what deliberately was not

The question this started with was whether the AArch64 CoW machinery could be
reused. It cannot, and the reason is worth recording: `akuma-exceptions`' CoW
break is ~500 lines welded to `akuma-exec` — address-space-owner resolution,
`as_lock`, lazy-region lookup, `CLONE_VM` thread-group handling — and almost all
of that bulk is **multi-thread races**, which amd64 at SMP=1 with no threads
does not have. Porting it would import the complexity without the problem.

What *is* shared is the **decision**, and on the AArch64 side it was never
written down in one place: four separate `cow_ref_get(pa) == 0` tests spread
across the file, each answering a slightly different question.

`crates/akuma-cow` is that decision as a pure function — `no_std`,
`#![forbid(unsafe_code)]`, **zero dependencies**, 8 host tests:

```rust
pub struct CowFault { pub pte_writable: bool, pub marked: bool, pub refs: u16 }
pub enum CowAction { Retry, Fault, TakeInPlace, Copy }
```

Four situations that need different answers, each of which is silent when
answered wrongly:

| situation | wrong answer | what you see |
|---|---|---|
| another thread already repaired the page | kill it | `SIGSEGV` on a write the page table *grants* |
| read-only on purpose (`mprotect`) | copy and grant | `mprotect(PROT_READ)` silently stops working |
| last holder | allocate and copy | correct, and needlessly slow on the commonest path |
| genuinely shared | grant in place | two processes silently share one page |

The **order** of the tests is the substance and has its own test:
`pte_writable` outranks everything, `marked` outranks `refs`. A reordering still
compiles and still passes every single-condition test.

### `marked` is not `refs > 0`

The marker is a property of the **PTE**, not the share count. It has to be: a
read-only page and a CoW-demoted page are byte-identical otherwise, and amd64
has no region table to consult instead — the PTE is the only record there is.
Deciding from the refcount alone promotes an `mprotect`ed page the moment its
frame happens to be shared, which is exactly
`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`.

amd64 carries it in PTE bit 9 (`AVL`, ignored by the CPU). **AArch64 has no such
bit today** and tests `cow_ref_get(pa) > 0`, so an adapter there would pass
`marked: refs > 0` — a **pinned divergence**, recorded in the crate rather than
hidden, and the reason that kernel currently cannot tell those two cases apart.

## The amd64 mechanism

- **`Prot` gained `cow`**, with `Prot::cow()` as the only constructor — it clears
  `write`, because a page that is both writable and CoW never faults, so the
  sharing never breaks and two address spaces diverge silently.
- **`prot_in` decodes the bit back.** Dropping it there would silently un-mark
  every page on the *second* fork of a process: the grandchild would share
  frames with no marker and take a fatal fault on its first write.
- **`fork_from` is a share loop**, demoting **both** spaces. Demoting only the
  child leaves the parent writing straight through to memory the child can see
  change — the entire point of CoW, missed. A page that is already read-only and
  *not* marked (`.rodata`, `mprotect`) is left exactly as it is in both.
- **The fault arm** sits after demand paging and before the user-copy fixup, and
  both orderings matter: a lazy page must be populated before anyone asks
  whether it is shared, and a `copy_to_user` landing on a CoW page is a
  legitimate write to break sharing for, not an `EFAULT` to hand back — sending
  it to the fixup would make `read(2)` into a forked child's buffer fail with no
  explanation.
- **`TakeInPlace` is not just an optimisation.** The common shape is `fork` then
  immediately `execve`; the child touches a few pages and throws the space away,
  leaving the parent sole owner of everything. Copying there is pure waste.

## Three teardown sites had to change, and all three were use-after-frees

Freeing raw was correct only while `fork` copied eagerly. The moment sharing
exists, every release must go through `cow_ref_dec` and free only on the last
reference — an untracked frame answers `true`, so an unshared process is
unaffected:

- `loader::free_all_frames` (process teardown)
- `mm::release_anon_frames` (the mmap window at exit)
- `mm::sys_munmap`

Missing any one frees a page the *surviving* process is still reading, and it
surfaces nowhere near the exit that caused it. Fixing them was what took the box
from "wedged after four commands" to working.

## The ceiling that was not memory

With CoW in, `fork` still failed at **~500 cumulative forks** — with 1.5 GB
free, the process table showing two entries, and memory *perfectly flat*. Three
diagnostics were added before the cause was found, and two of them never fired,
which was itself the information: it was neither slot exhaustion in `PROCS`/
`SPAWN` nor a failure inside `fork_from`.

It was `amd64/src/sched.rs`: **`finish()` sets `State::Finished` and nothing ever
set `Unused` again.** `MAX_TASKS = 512` was therefore a ceiling on *total
processes for the life of the boot*, not on concurrency. `sh` reported
`can't fork: Out of memory`, naming the one resource that was not exhausted.

This is **amd64-only** — that scheduler is its own; amd64 depends on neither
`akuma-scheduler` nor `akuma-threading`. The AArch64 side recycles properly
(`akuma-slot-table` frees `RETIRED` slots after a cooldown, and
`set_slot_reap_callback` fires at TERMINATED→FREE).

The fix is what the file's own comment had already proposed: a `Finished` task is
never chosen by the scheduler (it only picks `Runnable`), so it is never resumed
and the frame parked on its stack is dead. The allocator now accepts a slot that
is `Unused` **or** `Finished && !on_cpu && !daemon`. `Task` gained `stack_base`
and `trap_base` so a recycled slot **reuses the two 32 KiB stacks it already
owns** — without that, reclaiming would `leak()` two fresh stacks each time and
turn a slot leak into a memory leak, strictly worse than the exhaustion it fixes.

Diagnostics kept: `fork` now names which table is full and how full, and
`fork_from` reports which of its two failure paths it took. A bare `ENOMEM` that
reaches the user as "Out of memory" while naming the wrong resource is what cost
the time here.

## Measured

| | before | after |
|---|---|---|
| forks before failure | ~500 | **2000+, no failure** |
| memory drift over the run | n/a (died) | **0 KiB** |
| time per 200 forks | — | **0.4 s, flat** |

Correctness, which matters more than the counts:

```
$ sh -c 'x=parent; (x=child; echo in-sub=$x); echo after=$x'
in-sub=child
after=parent          <- the child's write did NOT reach the parent
$ sh -c 'x=inherited; (echo in-sub=$x)'
in-sub=inherited      <- and the child does see what the parent had
```

Also: nested forks, `ps`, dynamic linking, and `fork` *through* the dynamic
linker all still work. 240/0 boot self-tests, zero faults and zero fork failures
across the whole 2000-fork run. Tier 1 gate: four clippy profiles clean, 1315
host tests, 0 failed.

## Known limits

- **SMP=1 only, by construction.** `smp.rs` states `invlpg` is core-local with no
  TLB shootdown. `fork_from` demotes the parent's live PTEs, and at SMP>1 another
  core can hold a stale writable translation. CoW needs a shootdown before SMP>1
  — this is not a bug to find later, it is a stated precondition.
- **No threads**, so none of the `cowstale` race class exists here yet. The
  `Retry` arm is implemented and counted anyway, because it costs nothing and the
  alternative when threads arrive is a spurious `SIGSEGV`.
- The eager-`mmap` window and its global bump allocator are untouched;
  `release_anon_frames` still walks `[MMAP_BASE, NEXT_VA)` page by page at every
  non-forked teardown, which grows with uptime.

## Background

- `docs/archive/AKUMA_USER_SPACE_LEDGER.md` — the per-frame refcount this needed.
- `docs/archive/AKUMA_AMD64_DYNAMIC_LINKING.md` — the step before this one.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why the marker is a PTE bit.
