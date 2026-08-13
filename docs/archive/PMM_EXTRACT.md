# Extracting the PMM into `akuma-pmm`

**Date:** 2026-08-13. **Status: PLAN — nothing extracted yet.** Every count and
dependency below was measured out of the tree on that date; the design decisions
are recorded with their reasoning so they can be re-argued rather than
re-discovered.

Prompted by the question "would you advise extracting pmm into a crate?" and by
the observation that answered it: **every extraction so far has landed in
`akuma-exec`**, which is now 23.8k lines, because it is the only crate holding
kernel-ish state. `akuma-exec` has become the default destination by absence of
alternatives. The PMM is the first genuine decomposition available, and the reason
is not aesthetic.

---

## 1. The boundary already exists — you are paying for it at runtime

`ExecRuntime` (`crates/akuma-exec/src/runtime.rs`) is **46 function pointers**,
and **15 of them are the PMM**:

```
alloc_page              alloc_page_zeroed        alloc_pages_contiguous_zeroed
free_page               free_pages_contiguous    pmm_stats
free_count              total_count              track_frame
cow_ref_inc             cow_ref_dec              cow_ref_get
cow_fault_lock          cow_fault_unlock         is_memory_low
```

That indirection exists for exactly one reason: so `akuma-exec` does not have to
depend on the PMM, which lives in the kernel binary crate. Make the PMM a crate
and the dependency becomes ordinary, the 15 pointers disappear, and three of them
(`alloc_page_zeroed`, `track_frame`, `cow_ref_inc`) are on the **fault path**.

This is the same argument Phase 3 of
[`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
already used once, when it deleted the `NetHal` runtime indirection because it
"cost a spinlocked struct read on the per-packet DMA path to reach two identity
functions." Same shape, larger scale, and this time the indirection also blocks
host testing of the tree's most consequential allocator.

Three of those 15 are already dead weight:
[`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §5 found `cow_ref_dec`,
`cow_fault_lock` and `cow_fault_unlock` are **never called through the runtime
table at all** (`cow_ref_inc` at `process/mod.rs:265` is the only live one).

## 2. Feasibility: no cycle, and the leaf already has what PMM needs

Measured outward references from `src/pmm.rs` (1,525 lines):

| count | reference | after extraction |
|---:|---|---|
| 18 | `crate::irq::with_irqs_disabled` | `akuma_primitives::irq` ✓ already there |
| 8 | `crate::config::*` | `PmmConfig`, registered like `ExecConfig` |
| 5 | `akuma_exec::mmu::phys_to_virt` | `akuma_primitives::addr` ✓ already there |
| 3 | `crate::allocator::reclaim_to_pmm` | hook (§4) — genuine pmm↔heap cycle |
| 3 | `akuma_exec::threading::current_thread_id` | `akuma_primitives::preempt::current_tid` ✓ |
| 2 | `crate::console::*` | `akuma_primitives::console` ✓ (`safe_print!`) |
| 2 | `akuma_exec::memmath::poison_word*` | **comes back into `akuma-pmm`** (§5) |
| 1 | `akuma_exec::mmu::PAGE_SIZE` | a const; move or re-export |
| 1 | `akuma_exec::process::{table, reclaim, reclaim_clean_file_pages}` | hooks (§4) |
| 1 | `crate::file_page_cache::shrink` | hook (§4) |

**The `Spinlock` question is settled: it comes from `spinning_top`, an external
crate** (`src/pmm.rs:9`), not from `akuma-exec`. So there is no cycle through the
lock type — the single most likely blocker, and it is absent.

RAM bounds (`mmu::ram_base`/`ram_end`) are set once at boot from the DTB and read
as plain atomics with a documented pre-`init` fallback; PMM can take them as
`init` parameters instead of calling back into `akuma-exec`.

## 3. The layering

```
  akuma-primitives     addr, irq, once, preempt, console, clock        (leaf, no deps)
          ↓
  akuma-pmm            BitmapAllocator, FrameTracker, CoW refcounts,
                       UAF quarantine + poison codec, free/CoW ledgers,
                       the user-page reserve, AND the pressure escalation
          ↓
  akuma-exec           mmu, process, threading, elf — depends on akuma-pmm
                       directly; ExecRuntime loses 15 fn pointers
          ↓
  src/                 registers PMM's 4 hooks; owns the heap (talc), the
                       file-page cache, and boot
```

### What moves

The 20 statics that are the PMM's actual state:

| group | statics |
|---|---|
| allocator | `PMM` (BitmapAllocator), `TOTAL_PAGES`, `ALLOCATED_PAGES` |
| frame tracking | `FRAME_TRACKER` |
| CoW | `COW_REFCOUNTS`, `COW_EVER`, `COW_EVER_BASE`, `COW_LEDGER_{PA,META,SEQ,NEXT}` |
| quarantine | `QUARANTINE`, `QUAR_PRESENT`, `UAF_DETECTED`, `PREMATURE_FREES` |
| free ledger | `FREE_LEDGER_{PA,META,NEXT}`, `DOUBLE_FREE_COUNT` |
| dead | `COW_FAULT_LOCK` — **delete it**, it locks nothing (`COW_PILE_AUDIT.md` §5) |

…plus the 36 `pub fn`s over them, of which the interesting ones are
`alloc_page`, `alloc_page_zeroed`, `alloc_page_zeroed_user`, `alloc_pages_zeroed`,
`alloc_pages_contiguous_zeroed`, `free_page`, `cow_ref_{inc,dec,get}`,
`track_frame`, `stats`, `quarantine_drain_all`, `report_poison_value`.

### What stays in `src/`

The kernel heap (`src/allocator.rs`, talc + `PmmOomHandler`), the shared
file-page cache, boot/DTB parsing, and the four hook *implementations*.

## 4. The escalation goes **into** the crate, and only cold collaborators become hooks

This is the design decision that changed during review, and the reasoning matters
because it inverts an earlier one.

The first proposal kept `alloc_page_zeroed_user`'s pressure escalation in `src/`,
on the grounds that it names `process::reclaim` and `file_page_cache` and would
otherwise invert the dependency. The objection to hooking those was "it adds
fn-pointer calls on the fault path."

**That objection was imprecise, and applying it here was wrong.** It is true of
`free_count()` — the gate, which runs on *every* user page allocation, healthy or
starving. It is **not** true of the reclaim collaborators: those run only after
`user_alloc_would_starve` is already true, at most once each per starving
allocation, and every one of them does hundreds of microseconds of real work
(sweeping 512 pages, dropping entire address spaces). An indirect call is free
next to that.

So the escalation belongs **inside `akuma-pmm`**, which makes the hot call
(`free_count`) *internal and direct* and turns only the cold steps into hooks:

| hook | implementation in `src/` | fires |
|---|---|---|
| `heap_reclaim() -> usize` | `allocator::reclaim_to_pmm` | step 1, pressure only |
| `drain_retired() -> usize` | `process::reclaim::drain_retired_under_pressure` | step 2, pressure only |
| `evict_clean_file_pages(n) -> usize` | `process::reclaim_clean_file_pages` | step 3, pressure only |
| `shrink_page_cache(n) -> usize` | `file_page_cache::shrink` | step 3b, pressure only |

Net indirection: **−15 (ExecRuntime) +4 (cold PmmHooks) = −11**, and every hot
path — `free_count`, `alloc_page*`, `cow_ref_*` — becomes a direct call.

### These hooks must be mandatory-registration, not optional

§6.1 of the trimming doc draws the line at "a *diagnostic* hook that degrades to
nothing is free, but a **wake** hook that silently no-ops is a hang." A *reclaim*
hook is worse than a hang: if it silently no-ops, the escalation skips a step that
would have freed memory and **SIGSEGVs the process instead** — an invented OOM
that is indistinguishable from a real one in the log.

So: `Registered<PmmHooks>` with `require()` semantics, registered in `src/main.rs`
next to `ExecRuntime`, and never an `Option` that shrugs. The ordering is
satisfiable because the hooks are needed only under *user* memory pressure, which
cannot occur before userspace exists.

## 5. This corrects `memmath`'s membership

[`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
§5.11 moved the quarantine poison codec (`POISON_MAGIC`, `poison_word`,
`poison_decode`, `poison_word_frame`) and the user-page reserve
(`USER_PAGE_RESERVE`, `user_alloc_would_starve`, `user_readahead_budget`) into
`akuma_exec::memmath`. Both are **PMM concepts**; they went to `akuma-exec`
because no PMM crate existed and `src/` was host-unreachable.

They should migrate to `akuma-pmm` when it lands, leaving `memmath` with the fork
copy-range math and the mapping predicates — i.e. the things whose consumer really
is `akuma-exec`. Treat `memmath` as a correct waypoint under a constraint that
this change removes, not as a mistake and not as a final home.

## 6. The payoff that is not about line counts

`akuma-pmm` would be host-testable, and the things it owns are precisely the ones
whose bugs have cost the most:

- **CoW refcount accounting** — the §5.6 underflow (one reference per *address
  space*, not per VA) has never had a unit test; it was found in production, fixed
  three times, and is still only covered by boot tests.
- **The quarantine / poison codec** — already partly host-tested via `memmath`,
  but the quarantine *itself* (512-slot ring, `QUAR_PRESENT` collisions,
  drain-under-pressure) is not.
- **The bitmap allocator** — alloc/free symmetry, contiguous-run search, the
  fragmentation behaviour that `heap_grow_backoff` exists to survive.
- **The escalation** — five steps with a re-check between each, currently
  **untested in either place**, because its own boot test says "actually draining
  RAM to the reserve is unsafe inside the boot suite"
  (`docs/reference/subsystems/memory.md` → "OOM decision map"). With the four
  hooks injectable, every step and the cooling-cooldown gap become directly
  testable: register a `drain_retired` that returns 0 and assert the escalation
  does not reach `GiveUp` while memory is parked.

### It also subsumes the host-test audit's biggest scaffolding item

[`HOST_TESTS_AUDIT.md`](HOST_TESTS_AUDIT.md) §5 recommends, as its highest-value
scaffolding, an **arena-backed fake `alloc_page_zeroed`** in `test_support` —
because `ensure_test_runtime`'s `alloc_page_zeroed: || None` makes
`UserAddressSpace::new()` return `None`, which kills `make_test_process`, which
**105 registered boot tests** depend on.

If the PMM is a crate, you do not fake it — **you run the real PMM over a host
arena.** `phys_to_virt` is `#[inline(always)] paddr as *mut u8`, the identity, so
real host-heap addresses work as `PhysFrame`s. That replaces a fake with the
actual allocator under test, and it is strictly better coverage than the fake
would give.

## 7. Sequencing, and the risk

**Do this after the CoW merge** ([`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §8.1),
not before or during. Both touch the `cow_ref_dec` protocol, and the merge's
entire verification story is *"nothing changed"* — moving the refcount table to
another crate inside that window destroys the ability to attribute a regression.

Suggested order once the merge has landed and been verified:

1. `PmmConfig` + the 4 hooks, still inside `src/` — pure plumbing, no move. Verify.
2. Move the allocator core + `FrameTracker` + ledgers + quarantine into
   `akuma-pmm`, re-exported from `src/pmm.rs` so no call site changes. Verify.
3. Move the CoW refcount table and delete `COW_FAULT_LOCK`. Verify.
4. Move the escalation in; make `free_count` internal. Verify.
5. Delete the 15 `ExecRuntime` fields and point `akuma-exec` at the crate. Verify.
6. Migrate the poison codec and reserve out of `memmath` (§5).
7. Host tests: allocator, refcounts, quarantine, escalation.

**The risk is real and should be stated plainly:** `src/pmm.rs` is the file behind
the page-table UAF, the premature-free class, and the cargo null-`Rc` defect that
is still open. It is the last file in the tree one would choose to move casually.
The mitigations are that every step above is a re-export-preserving move (no call
site changes until step 5), the gate is
[`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
run at SMP=1 **and** SMP=4 with `cowstale`/`bssfork` after each step, and the
whole point is that the code lands somewhere its invariants can finally be
asserted by a test instead of by a comment.

## 8. Acceptance milestone: `bssfork spread=1`

**This is the extraction's acceptance gate, and it is currently RED on every
branch — including `main`.** Diagnosing it is part of the work, not a precondition
for starting.

`userspace/forktest/c_stress/bssfork.c` is the narrowest probe of the CoW
fault path in the tree: T threads incrementing adjacent `.bss` counters while the
main thread forks R times. `spread=0` puts every counter on one page so the
threads contend on the same CoW break; **`spread=1` gives each thread its own
page**, and its own header describes it as the control — *"Use it to tell 'this
load is too much for the machine' from 'this load hits the contended-fault
path'."*

Measured 2026-08-13 at SMP=4, twice per tree, on unmodified `git worktree`
checkouts:

| tree | `bssfork` (spread=0) | `bssfork spread=1` |
|---|---|---|
| `main` (b585aed) | PASS, `failures=0` | **FAIL — `failures=7`**, `thread=7 [never ran] ticks=0`, total ticks 5.8M / 3.2M |
| `trim-some-more-fat` (1a5a266) | PASS, `failures=0` | **FAIL — `failures=8`, `ticks=0`** — *no thread runs at all* |

Two separate facts, and both matter:

1. **The control does not work.** Anything that cites `bssfork spread=1` to
   exonerate a CoW change is citing a test that fails on a pristine tree. This is
   already flagged in
   [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)'s
   Tier 3 table so nobody uses it as a gate in the meantime; plain `bssfork`
   (spread=0) passes on both trees and is the control to use today.
2. **It got worse across 21 commits.** `main`'s 7-of-8 with non-zero total ticks
   means one thread ran; the branch's 8-of-8 with `ticks=0` means none did. That
   is an unexplained regression in a **CoW-plus-threads** workload, i.e. exactly
   the class this extraction touches.

### Why it belongs to this work rather than its own investigation

The failure mode — threads never scheduled, `ticks=0` — is *not* obviously a CoW
bug; `spread=1` deliberately removes contended CoW faults. The plausible causes
are thread admission, per-page demand paging of eight separate `.bss` pages, or
frame-allocation pressure with eight thread stacks plus eight private pages. All
three run through the allocator and the frame tracker this extraction moves, and
**none of them is testable today** for the reason §6 gives: the allocator is
unreachable from a host test.

So the honest framing is that `spread=1` is a symptom the extraction is expected
to make *diagnosable*, and the acceptance criteria are:

| # | Criterion |
|---|---|
| 1 | **No further degradation at any step.** `spread=0` stays PASS `failures=0`; `spread=1` must not regress past the branch's current `failures=8, ticks=0`. Re-measure after each of §7's seven steps — a step that moves it has found something |
| 2 | **The 8/8-vs-7/8 gap is explained**, not merely closed. `main` runs one thread and the branch runs none; whatever accounts for that difference is a real finding about frame or thread admission and must be written down |
| 3 | **`spread=1` reaches `failures=0`, or its failure is proven to be the probe's own** — the header records that an earlier version of the never-ran check failed on real Linux, which was "the probe being wrong, not the kernel". Recheck that against Alpine on arm64 (the calibration command is in the header) before assuming the kernel is at fault |
| 4 | **A host test covers whatever the diagnosis turns out to be.** If it is allocation pressure, the arena-backed test from §6 must reproduce it without a VM. That is the whole point of the extraction |

Criterion 4 is the one that makes this a milestone rather than a chore: if the
extraction lands and `spread=1` is still only reproducible by booting QEMU at
SMP=4, the extraction did not buy what it was supposed to buy.

## Background

- [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) — the CoW-break paths, the dead
  `cow_fault_lock`, and §8.1's scoped merge that must land first
- [`HOST_TESTS_AUDIT.md`](HOST_TESTS_AUDIT.md) — the 553 boot tests and why the
  arena is the bottleneck
- [`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  §5.11 (the `memmath` move this revises), §5.10 (`lto`/inlining), §6.1 (the
  injection principle and the hook-degradation line)
- [`../reference/subsystems/memory.md`](../reference/subsystems/memory.md) —
  "OOM decision map": the escalation, the six recovery mechanisms, the constants
- [`OOM_KILL_DEFERRED_RECLAIM_GAP.md`](OOM_KILL_DEFERRED_RECLAIM_GAP.md) — why the
  retired-process collector exists at all
