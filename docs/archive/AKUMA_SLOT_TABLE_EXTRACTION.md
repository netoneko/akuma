# `process/table.rs`'s slot store → `akuma-slot-table`

**Date:** 2026-09-02
**Branch:** `oof-part-2`
**Item:** `AKUMA_EXEC_AUDIT.md` §6 step D (and §6.E group 1) — the process-table
half of splitting `akuma-exec` toward `#![forbid(unsafe_code)]`.

**Result:** the lock-free per-slot `*mut Process` store — the
FREE/ACTIVE/RETIRED state array, the `[AtomicPtr<Process>; N]`, the per-slot
reuse generation, the retire timestamp, and **every dereference of those
pointers** — is a generic `SlotTable<T, N>` primitive that never names
`Process`. `akuma-exec` goes from **25 to 7** `unsafe` sites; the new
`akuma-slot-table` carries **11**, behind one stated contract in a ~400-line
crate. This is the `akuma-locks-rw-cell` / `akuma-gic` move: extract the
obligation into a crate that owns it, keep it generic so it is discharged once.

---

## 1. Why a primitive crate and not "leave it `unsafe`"

`AKUMA_EXEC_AUDIT.md` §6.D offered two options for `table.rs`: a small crate
that just can't `forbid` (like `akuma-gic`), or a generic primitive. The
generic won for the reason `akuma-locks-rw-cell` did — the thing being vouched
for is a **property of the slot mechanics** (a pointer is immutable within a
generation; a `Box` is freed only after a cooldown), not a property of
`Process`. Stated once, generically, it covers `table.rs` and any future
per-slot store the tree grows, and `Process` never has to be exported to a crate
below `akuma-exec` to get it there.

The distribution made it viable: of `table.rs`'s ~19 `unsafe` sites, **all but
`with_process_exclusive`** were one of two shapes — a raw deref of
`PROCESS_SLOTS[i]` during a scan, or the retire/reclaim pointer-swap-and-drop
mechanics. Both are `SlotTable`'s subject matter.

## 2. `SlotTable<T, const N: usize>` — the API

Four fixed arrays, `Sync` for any `T` (its fields are atomics; `AtomicPtr<T>` is
`Sync` unconditionally), no `Drop` — it lives in a `static` and every occupant's
`Box<T>` is released through `reclaim_retired`.

| method | what `table.rs` used it for |
|---|---|
| `try_claim(Box<T>) -> Result<usize, Box<T>>` | `register_process` — CAS `FREE→ACTIVE`, publish pointer `Release`; hands the box back on a full table (so `register_process` drops it instead of `Box::from_raw`) |
| `retire(retire_at: impl FnOnce() -> u64, pred, on_retired: impl FnOnce(usize, &T)) -> bool` | `unregister_process` — scan ACTIVE, first `pred` match, CAS `ACTIVE→RETIRED`, stamp the time **after** the winning CAS, run the domain teardown with the slot still live |
| `reclaim_retired(now, cooldown, ignore_cooldown, on_free: impl FnMut(usize)) -> usize` | `reclaim_retired_processes{,_force}` — per eligible RETIRED slot: swap pointer to null, bump generation **while still RETIRED**, store FREE, clear stamp, `on_free(i)`, drop the `Box` |
| `ref_if_current(i, expected_gen) -> Result<&T, SlotMiss>` | the identity cache — state, then generation, then pointer; `SlotMiss::{Inactive, StaleGen, Null}` map 1:1 to the three fallback counters |
| `active_ref(pred) -> Option<&T>` | `lookup_process_shared` (via `table::active_process_ref`) — IRQ-masked scan, shared borrow |
| `active_ptr_locked(pred) -> Option<*mut T>` | `get_process_ptr` (wrapped in `with_irqs_disabled`) — the one raw-pointer accessor still used, by `entry_point_trampoline` (a Group 2 site) |
| `with_active_mut(pred, f)` | `with_process` — IRQ-masked `&mut T` closure |
| `unsafe active_exclusive(pred, f)` | `with_process_exclusive` — **no** mask; the obligation is forwarded to the caller verbatim |
| `for_each_active` / `find_active` / `*_locked` | `for_each_process`, `find_process`, `collect_pids`, `collect_process_info`, `process_count`, `count_process_states`, `identity_store_locked` |
| `is_active` / `generation` / `active_count` / `retired_count` | scalars — `retired_process_count`, `test_hooks` |

### The one stated contract

At the top of `lib.rs`, covering every borrow-returning method:

> A slot's `Box<T>` is dropped **only** by `reclaim_retired`, and only after a
> cooldown long enough that no core can still hold a pointer obtained from a
> scan that has since returned. Within one generation a slot's pointer is
> immutable — written exactly once by `try_claim` (after the winning CAS) and
> nulled exactly once by `reclaim_retired` (before the generation bump).

On one core, an IRQ mask across a scan makes "since returned" true —
`reclaim_retired` runs in EL1, so a masked core cannot be running it. Across
cores that is the caller's lock (the BKL, in `akuma-exec`). The crate provides
the mask (via `akuma-primitives`); the cross-core exclusion and the cooldown
are the consumer's. This is exactly what `table.rs`'s scattered SAFETY comments
said — now stated once.

### Orderings preserved verbatim

- `try_claim`: CAS `SeqCst` on success / `Relaxed` on failure, then `slots[i]`
  store `Release`.
- `retire`: CAS `AcqRel`, `retire_time` store `Release`.
- `reclaim_retired`: pointer swap `AcqRel`, **generation `fetch_add(AcqRel)`
  while the slot is still RETIRED**, then `FREE` store `Release`. Every reader
  rejects a non-ACTIVE state before it reads the generation, so none can pair
  ACTIVE with the pre-bump stamp. This is the whole Finding-B safety argument
  (`IDENTITY_CACHE_SMP_REVIEW.md`) and it is unchanged.
- `ref_if_current`: state `Relaxed`, generation `Acquire`, pointer `Acquire` —
  the same sequence `identity_get` did inline.

`retire` takes the timestamp as a **closure** rather than a value so it is read
*after* the winning CAS, matching the old `RETIRE_TIME[i].store((runtime().uptime_us)())`
placement (a value argument would have read it before the scan).

## 3. The identity cache lost its cached pointers

The interesting part. `ThreadIdentity` cached, per thread slot, both an
`own_slot: AtomicU16` **and** an `own_ptr: AtomicPtr<Process>` (same for the
tgid half). `identity_get` validated state + generation, then dereferenced
`own_ptr`.

`own_ptr` is now **gone**. `identity_get` calls
`SlotTable::ref_if_current(slot, stamped_generation)`, which loads
`PROCESS_SLOTS[slot]` itself. This is sound *and behaviour-identical* because
the comment at the old deref site already argued it:

> `PROCESS_SLOTS[i]` is written once per generation and nulled once, so within a
> generation the slot's pointer is immutable and a matching stamp already proves
> the cached pid is right.

If the generation matches, the live `PROCESS_SLOTS[slot]` **is** what `own_ptr`
held. Caching it separately bought nothing and cost two `AtomicPtr` fields plus
a publication-ordering obligation.

The publication point moved with it: `identity_store_locked` used the `own_ptr`
`Release` store to publish `own_pid` / `own_slot` / `own_gen`. With no pointer,
the `own_slot` store becomes the `Release` (it was already loaded `Acquire` by
`identity_get`), and the generation is still stamped before it. `identity_clear_locked`
and `test_hooks::stamp_unresolved` drop their `*_ptr` writes the same way.

Net: `ThreadIdentity` is `own_pid`, `own_slot`, `own_gen`, `tgid`, `tgid_slot`,
`tgid_gen`, `repair_attempts` — no raw pointers, and `identity_get` holds no
`unsafe`.

## 4. Two incidental `unsafe` sites also went

`collect_process_info` built a `[MaybeUninit<T>; MAX_PROCESSES]` with
`assume_init` on the way in and out — two `unsafe` blocks unrelated to the slot
store, present only because the author reached for `MaybeUninit`. The function's
bound is already `T: Copy + Default`, so `[T::default(); MAX_PROCESSES]` is a
safe array-repeat that does the same thing. Removed.

## 5. What moved, what stayed

| | where | `unsafe` |
|---|---|---:|
| **moved** → `akuma-slot-table/src/lib.rs` | the 4 arrays, `try_claim`, `retire`, `reclaim_retired`, `ref_if_current`, the scan/iteration accessors, `active_exclusive`, the scalars; 9 host tests | 11 |
| **stayed** → `akuma-exec/src/process/table.rs` | `MAX_PROCESSES`, `NEXT_PID`, `THREAD_PID_MAP`, the whole identity cache, `pid_for_thread`/`thread_for_pid`, the reclaim-hook calls, `mark_thread_terminated` — all domain logic, calling `PROCESS_TABLE.<method>` for each deref | 1 (`with_process_exclusive`'s forwarding block) + the `unsafe fn` signature |

`akuma-slot-table` depends only on `akuma-primitives` (`with_irqs_disabled`).
No cycle — nothing `akuma-primitives` needs points back.

`with_process_exclusive` stays `unsafe fn` and holds one `unsafe` block
(forwarding to `active_exclusive`, which vouches for nothing). Both survive only
because **Group 2** (`AKUMA_EXEC_AUDIT.md` §6.E — the execve/first-run exclusive
`&mut Process` window, Phase 7f) has not removed its three callers yet. A
generic `SlotTable` can move the deref; it cannot remove the obligation.

## 6. Call-site churn: zero outside `table.rs` + one line in `children.rs`

Every public `table::*` signature is unchanged (`with_process`,
`get_process_ptr`, `for_each_process`, `collect_pids`, …), so no consumer moved.
`table::slot_state`, `SLOT_STATES`, `PROCESS_SLOTS`, `SLOT_GEN`, `RETIRE_TIME`
and `try_claim_free_slot` had **no** external references (only doc-comment
mentions), so deleting them was free. `children.rs`'s `lookup_process_shared`
swapped its `get_process_ptr(pid).map(|ptr| unsafe { &*ptr })` for the new safe
`table::active_process_ref(pid)` — one line.

## 7. Verification

**Host:** `akuma-slot-table` 9 tests (claim/retire/reclaim roundtrip, box
dropped only by reclaim, full table hands the box back, generation tracking,
ignore-cooldown, iteration visits only active, `with_active_mut`, **racing
reclaimers free each slot once** and **racing claimers get distinct slots** —
200 rounds × 4/8 `std::thread`s each). `akuma-exec` 86 tests / 0 failed. Full
workspace `cargo test --target $HOST` — 0 failed. Clippy clean on both crates.

**Boot self-test suite** (`MEMORY=2048M`, INSTANCE=3): **0 FAIL, 0 PANIC**. The
tests that exercise this code specifically all PASS —
`identity_lazy_restamp`, `identity_recycled_slot_rejected` (the generation
guard), `epilogue_identity_revalidated`, `fork-bkl-drop`,
`pmm_conserved_across_spawn_exit_reap` (0-page drift), `retired_reclaim_*`,
`thread_slot_reclaim_on_spawn`, `slot_recycling`. SSH round-trip + a 20-way
fork exercise: clean.

**SMP=4 `forktest_smp_matrix.py`:** all 7 configs (basic, mmap, file_io, signal,
goroutine_stress, combined_light, combined_heavy) **PASS** — 0 `[BKL] RECOVERED`,
0 `[PANIC]`, 0 `WILD-DA`.

Not run: the self-host kernel build.

## Background

- [`AKUMA_EXEC_AUDIT.md`](AKUMA_EXEC_AUDIT.md) §6.D / §6.E — the plan, and the
  two groups still between `akuma-exec` and `forbid`.
- [`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md) Finding B — the
  generation-vs-recycle argument `reclaim_retired` and `ref_if_current` preserve.
- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md)
  — the deferred RETIRED→FREE reclamation and its cooldown, which is the
  consumer's half of `akuma-slot-table`'s contract.
- [`AKUMA_EXEC_ADDRESS_SPACE_MERGE.md`](AKUMA_EXEC_ADDRESS_SPACE_MERGE.md),
  [`AKUMA_EXEC_USER_ACCESS_EXTRACTION.md`](AKUMA_EXEC_USER_ACCESS_EXTRACTION.md)
  — the sibling steps of the same split.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the
  "Not enforceable, and why" table `akuma-slot-table` joined.
