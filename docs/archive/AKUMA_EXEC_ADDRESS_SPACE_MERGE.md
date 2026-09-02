# `Process::address_space` + `as_lock` → `ProcAddressSpace`

**Date:** 2026-09-02
**Branch:** `oof-part-2`
**Parent:** `a6c7aef0`
**Closes:** `AKUMA_EXEC_AUDIT.md` §5 — the last two of the six `&self -> &mut`
field casts, both in `Process::with_address_space`. The crate now holds **zero**
such casts.

**One line:** `Process` carried its user address space as two fields — the data
(`address_space: UserAddressSpace`) and a lock guarding nothing beside it
(`as_lock: Spinlock<()>`) — and `with_address_space` bridged them with
`&mut *(addr_of!(self.address_space) as *mut UserAddressSpace)`. They are now one
field, `address_space: ProcAddressSpace`, which is `Spinlock<UserAddressSpace>`
plus a lock-free atomic mirror of the four scalars the fault path reads without
the lock. No cast, no `UnsafeCell`, no `unsafe` in the replacement.

---

## 1. Why the audit stopped here

`AKUMA_EXEC_AUDIT.md` §5c-bis removed four of the six casts the same way: put the
`Vec` *inside* the `Spinlock` instead of beside a `Spinlock<()>`. It deliberately
did **not** do the same to `address_space`, for two stated reasons:

1. **`with_address_space` has ~59 lock-free readers on the fault path.** The
   scalar getters `l0_phys` (42 call sites), `ttbr0` (11), `is_shared` (7),
   `asid` (3) are read without any lock, on the hottest path in the kernel.
   Naively wrapping `UserAddressSpace` in a `Spinlock` puts an acquire on every
   one of them, widening the `as_lock` deadlock surface the `Process` field doc
   already warns about (a nested IRQ hard-spinning for the BKL while this core
   holds `as_lock`, against a peer holding the BKL and waiting on `as_lock` in
   `munmap`).

2. **`as_lock` is not "a lock guarding nothing" the way `vm_lock` was.** Its
   field doc describes it as serializing "hardware page-table mutation for this
   address space across cores" — it guards the raw `write_volatile`s into the
   page tables inside `mmu::map_user_page*` / `UserAddressSpace::{un,}map_page`,
   which is real external state. It is a `Spinlock<()>` only because there was no
   Rust-visible data to put in it.

So §5c-bis called this "a change to the locking model, not a wrapper swap" and
left it for its own pass. This is that pass.

## 2. What the field audit actually found

The blocker in reason (1) dissolves once you look at `UserAddressSpace`:

```rust
pub struct UserAddressSpace {
    l0_frame: PhysFrame,                       // set in new()/new_shared(), never reassigned
    page_table_frames: Spinlock<Vec<PhysFrame>>,   // already interior-locked
    user_frames: Spinlock<BTreeMap<usize, u32>>,   // already interior-locked
    asid: u16,                                 // set in new()/new_shared(), never reassigned
    shared: bool,                              // set in new()/new_shared(), never reassigned
}
```

`grep` confirms: there is no `self.l0_frame =`, `self.asid =` or `self.shared =`
anywhere in the crate. The three scalars are **immutable after construction**.
The two aggregates are **already `Spinlock`-wrapped**. So:

- The `&mut self` on `map_page`, `unmap_and_free_page`, `update_page_flags`,
  `alloc_and_map`, `write_page_bytes`, `map_user_page_tracked*` etc. is **not a
  mutation requirement** — three of those methods carry
  `#[allow(clippy::needless_pass_by_ref_mut)]` and a comment saying exactly this:
  the `&mut` is a *capability token* proving the caller came through
  `with_address_space` and therefore holds `as_lock`. Demoting them all to
  `&self` (the first instinct) would silently reopen `map_user_page`'s contract.

- Every non-scalar `UserAddressSpace` access already runs **under `as_lock`**
  (the fault path in `src/exceptions.rs` takes `AsLockHold::new(&owner.as_lock)`
  before every `track_*`) **or holds an exclusive `&mut Process`** (fork child
  construction in `fork_process`, the ELF loader via `LoadedElf`). The four
  scalars are the *only* thing read while a writer might be running.

Therefore: mirror the four scalars as atomics on `Process`, and the objection to
`Spinlock<UserAddressSpace>` is gone — the fault-path readers never touch the
lock, and everything else was already serialized by it.

## 3. The shape

`crates/akuma-exec/src/process/address_space.rs`:

```rust
pub struct ProcAddressSpace {
    inner:  Spinlock<UserAddressSpace>,   // was `as_lock` + the data beside it
    ttbr0:  AtomicU64,                     // (asid << 48) | l0_phys — the TTBR0_EL1 value
    shared: AtomicBool,
}
```

- **Lock-free scalar mirror.** `ttbr0()`, `l0_phys()`, `asid()`, `is_shared()`
  are `Relaxed` atomic loads. One `AtomicU64` covers the first three (the TTBR0
  word packs ASID and L0 base); `shared` is its own bool.

- **`lock()`** returns an `AddressSpaceGuard` — `SpinlockGuard<UserAddressSpace>`
  plus an `IrqGuard` on `kernel_smp_shared`, field-ordered so the lock releases
  before DAIF is restored (the discipline the old `AsLockHold` documented). This
  is what `with_address_space` / `with_as_locked` / the old `AsLockHold` sites
  now use. **Zero `unsafe`.**

- **`get_mut()`** hands out `&mut UserAddressSpace` without locking, for the
  `&mut Process` build paths (fork child, `replace_image`) where the `Process`
  is not published and nothing can contend.

- **`replace(uas)`** swaps the inner value and stores the new `ttbr0`/`shared`
  in one step — the only mutator of the mirror for a live `Process`, called by
  `replace_image` under the `LifecycleGuard` with thread-group siblings already
  killed. A concurrent reader is a just-killed sibling or an unrelated process;
  the window is the same one today's plain field read has.

- **One-shot passthroughs** for the read-only `&self` methods (`translate`,
  `resident_pages`, `user_frame_count`, `tracks_user_frame`, …) so those call
  sites don't change. The *mutating* frame-tracking ops
  (`track_user_frame`/`track_page_table_frame`/`adopt_user_frame`/`remove_user_frame`/
  `invalidate_icache_for_page_va`) are **deliberately not passthroughs** — see §5.

`with_address_space` collapses to:

```rust
pub fn with_address_space<R>(&self, f: impl FnOnce(&mut UserAddressSpace) -> R) -> R {
    let mut g = self.address_space.lock();
    f(&mut g)
}
```

Both cfg arms become identical. The `not(kernel_smp_shared)` arm's
"single-core / BKL-serialized" cast is gone; on that config the spinlock is
simply always uncontended, because every caller holds the BKL first and the BKL
serializes them before they reach the spinlock. (The IrqGuard stays
`smp-shared`-only — on a single core the EL1 sync-exception entry has already
masked IRQs for the fault path, and syscall callers hold the BKL.)

## 4. `AsLockHold` deleted

`AsLockHold` (a `#[cfg(kernel_smp_shared)]` RAII hold of `Spinlock<()>` + IRQs)
had ~10 call sites in `src/exceptions.rs`, each of the shape:

```rust
#[cfg(kernel_smp_shared)]
let _asg = akuma_exec::process::AsLockHold::new(&owner.as_lock);
let (table_frames, installed) = unsafe { akuma_exec::mmu::map_user_page(...) };
for tf in table_frames { owner.address_space.track_page_table_frame(tf); }
```

— the `AsLockHold` held the lock, and `owner.address_space` was reached
*separately* as a plain field. Once the two are one field, that split cannot
work: the `track_*` calls have to go through the guard. Each site became:

```rust
let asg = owner.address_space.lock();
let (table_frames, installed) = unsafe { akuma_exec::mmu::map_user_page(...) };
for tf in table_frames { asg.track_page_table_frame(tf); }
```

The `#[cfg]` gate disappears — on `not(kernel_smp_shared)` this now takes a
(always-uncontended) spinlock where it took nothing, which is a few instructions
and no behaviour change (the BKL was and is the real serializer there).

Two exception-path sites used `with_as_locked(closure)` where the closure itself
reached `owner.address_space.track_*` — a pattern that would self-deadlock once
`track_*` is behind the lock the closure runs under. Both were rewritten to take
the guard explicitly and use it for the tracking.

## 5. A real deadlock the merge exposed and fixed

`CowRemap` (the CoW-break helper's remap strategy) had:

```rust
pub enum CowRemap<'a> {
    TakingAsLock(&'a Process),        // helper takes as_lock via with_address_space
    CallerHoldsAsLock(&'a Process),   // caller already holds it — helper must not re-take
}
```

The `CallerHoldsAsLock` arm called `owner.address_space.track_user_frame(new_frame)`
and `owner.address_space.remove_user_frame(...)`. As plain field accesses that
was fine. The instant `track_user_frame`/`remove_user_frame` sit behind the
lock, and the *caller* (`src/exceptions.rs`, the EL0 permission-fault CoW break)
holds `owner.address_space.lock()` across the whole break — those calls
**self-deadlock** on the non-reentrant `Spinlock`.

`CowRemap::CallerHoldsAsLock` now carries `&'a mut UserAddressSpace` — the
caller's own guard — and its arm calls the frame trackers on that. The caller
passes `&mut asg` (the guard, deref-coerced). This is strictly more correct than
before: the two operations that were lockless field pokes are now inside the
same hold as the rest of the break.

This is also why the mutating frame-tracking ops are **not** one-shot
passthroughs on `ProcAddressSpace`: those methods are called from fault / CoW
paths that already hold `lock()` across a `map_user_page` + track sequence, and a
self-locking passthrough there is exactly this deadlock. Those sites take
`lock()` and go through the guard; only the read-only `&self` methods get
passthroughs.

## 6. `fork_process`'s `parent_as` resolution

`fork_process` (the CoW pass) resolved the leader's `&Spinlock<()>` explicitly,
because a `CLONE_THREAD` sibling forking must take the **thread-group leader's**
lock, not its own fresh one (the fault handler resolves the same way via
`address_space_owner_pid_for_fault`). That resolution now yields
`&ProcAddressSpace`:

```rust
let parent_as: &ProcAddressSpace = {
    let owner_pid = address_space_owner_pid_for_fault().unwrap_or(parent_pid);
    if owner_pid == parent_pid { &parent.address_space }
    else { lookup_process_shared(owner_pid).map_or(&parent.address_space, |p| &p.address_space) }
};
```

`share_rw_range` / `cow_share_and_demote_range` take `parent_as: &ProcAddressSpace`
and do `let _asg = parent_as.lock();` per chunk. They only touch `parent_l0` (a
raw `*const u64`) inside the hold, so the guard is pure exclusion — exactly what
`AsLockHold::new(as_lock)` was.

One prefault path (`ensure_user_pages_present` in `process/user_access.rs`) can
have its lock owner (`address_space_owner_pid_for_fault`, the L0 owner) differ
from its frame-tracking target (`read_current_pid`, this thread's process) — only
for a vfork-child prefault, and only there. The two are the same `Process` for
every normal thread; the code special-cases pointer equality and takes one guard
in the common case, two distinct guards (leader's then owner's) in the rare one.

## 7. Call-site churn

| kind | count | change |
|---|---:|---|
| scalar reads `p.address_space.{l0_phys,ttbr0,asid,is_shared}()` | ~60 (26 prod) | **none** — hit the mirror |
| read-only `&self` methods `p.address_space.{translate,resident_pages,…}()` | ~15 | none — passthroughs |
| `&mut` methods on a fresh `Process`/`LoadedElf` | ~20 (mostly tests + fork + ELF loader) | `.get_mut().` |
| `AsLockHold::new(&owner.as_lock)` + separate `track_*` | ~10 (exceptions) | `owner.address_space.lock()` guard |
| `with_as_locked(closure)` where closure reaches the field | 2 (exceptions, `user_access`) | explicit guard |
| `Process` struct literals | 3 (`inherit_from`, `from_image`, `make_test_process`) | `ProcAddressSpace::new(uas)`, drop `as_lock:` |
| test `X.address_space = <uas>` | ~25 | `.replace(<uas>)` |
| `cow_share_and_demote_range` param | 2 fns + 9 call sites | `&Spinlock<()>` → `&ProcAddressSpace` |

## 8. Verification

Run at `26b57133` against the parent `a6c7aef0`, per
`docs/runbooks/verify-trim-fat-change.md`. All `MEMORY=2048`.

### `verify_trim.py` full A/B (tiers 1–3)

The two summaries are **identical except one line**:

```
33c33
< smp4.bkl_stuck: 102        (base a6c7aef0)
> smp4.bkl_stuck: 101        (mine)
```

`bkl_stuck` is load-driven and explicitly not compared by count. Everything else
matched byte-for-byte on both arms:

| | value (both arms) |
|---|---|
| `clippy.{release,extreme-size,devbox-smoltcp,devbox-rump}` | clean |
| `host.tests` / `host.failed` | 1102 / 0 |
| `smp{1,4}.booted` | True / True |
| `smp{1,4}.ex.*` (17 exercises each) | all `ok` |
| `smp{1,4}.fail_set` | empty / empty |
| `smp{1,4}.passed_marker` | 310 / 318 |
| `smp{1,4}.pass_marker` | 100 / 100 |
| `smp{1,4}.host_timejumps` | 0 / 0 (host quiet — readings trustworthy) |
| `smp{1,4}.stack_overflow` | 1 / 1 (the `stackstress` deliberate canary smash) |

### `mem_suite.py --port 2222 --no-build` (live VM)

`PASS (10/10 probes, 3 DIVERGE)` — `mmap_stress`, `mmapsum`, `mmap_file`,
`mprotectlb`, `mremapmove`, `madvshared`, `shmanon`, `cowstale`,
`eager_mprotect_probe` all `ok`; `smapsdirty` `ok, 3 DIVERGE` (the documented
green state).

### `forktest_smp_matrix.py` (SMP 2 & 4)

`# ALL TESTS PASSED` — **14/14**. Every run reported
`[BKL] RECOVERED: 0`, `[WATCHDOG]: 0`, `[PANIC]: 0`, `WILD-DA: 0`,
`[SGI-S POISON]: 0`.

### `verify_trim.py --tier 4` (redis `--test-memory` on devbox-smoltcp)

`redis.stage: ok`, `redis.vm_sigsegv: 0`, `redis.timejumps: 0`. The memtest
walked anonymous demand-paging / `USER_PAGE_RESERVE` / reclaim escalation and
verified the bytes — no `MEMORY ERROR DETECTED`, no OOM kill.

### `overlays/devbox-firecracker/{build,run}.sh` (KVM, `FC_MEM=2048`)

`lines=1613 PASSED=304 FAILED=1 POISON=0`, `[PASS]` count 100, no `[FAIL]`
marker, no `PANIC` / `[BKL] stuck` / `WILD`. The single failure is
`thread_slot_reclaim_on_spawn` (`hot_reclaim=108, want 0, in_cooldown_window=true`)
— the documented pre-existing timing sensitivity that fails on unmodified HEAD
too (`PAGE_TABLE_UAF_BKL_STORM.md` §7.3 note). `PASSED=304` vs the ~305 baseline
is a ±1 `passed_marker` move (a test reporting `INCONCLUSIVE`/`SKIPPED` instead
of `PASSED`), within documented noise; the signal — `FAILED=1` and it is that
one test, `POISON=0` — is clean.

## Background

- [`AKUMA_EXEC_AUDIT.md`](AKUMA_EXEC_AUDIT.md) §5, §5-bis, §5a, §5c-bis, §5c-ter —
  the six-cast finding, the "put the data inside the lock" design, and the four
  earlier fixes.
- [`PAGE_TABLE_UAF_BKL_STORM.md`](PAGE_TABLE_UAF_BKL_STORM.md) — the
  page-table-UAF class and the per-core `ACTIVE_L0`/`PREV_L0` free gate; the
  reason `l0_phys` changing under a lock-free reader had to be thought through
  (it changes only on `execve`, which already kills siblings first, and the TTBR
  gate — not this field — is what protects the frame-free).
- [`SMP_SHARED_M5_FAULT_LOCK_PLAN.md`](SMP_SHARED_M5_FAULT_LOCK_PLAN.md) — the
  BKL-free fault path that `as_lock` (now `address_space.lock()`) serves.
- `docs/runbooks/verify-trim-fat-change.md` — the A/B gate this change was run
  through.
