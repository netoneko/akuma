# Extracting the PMM into `akuma-pmm`

**Date:** 2026-08-13. **Status: PLAN — nothing extracted yet.** Every count and
dependency below was measured out of the tree on that date; the design decisions
are recorded with their reasoning so they can be re-argued rather than
re-discovered.

> **Updated 2026-08-13, after the §8.1 CoW merge landed** (its precondition, see §7).
> The merge moved three of this plan's numbers, and they are corrected in place below:
> `ExecRuntime` is now **44 fn pointers of which 13 are the PMM** (`cow_fault_lock`
> and `cow_fault_unlock` are deleted), so the deletion in §7 step 5 is 13, not 15, and
> the net indirection saving in §4 is −13 +4 = **−9**. `COW_FAULT_LOCK` is already
> gone, so §7 step 3 is only the table move. `memmath` gained `next_reclaim_step` /
> `ReclaimStep`, which §5 must migrate along with the poison codec and the reserve —
> the escalation's *decision* is already extracted and host-tested; what §4 moves into
> the crate is its *effects*.

Prompted by the question "would you advise extracting pmm into a crate?" and by
the observation that answered it: **every extraction so far has landed in
`akuma-exec`**, which is now 23.8k lines, because it is the only crate holding
kernel-ish state. `akuma-exec` has become the default destination by absence of
alternatives. The PMM is the first genuine decomposition available, and the reason
is not aesthetic.

---

## 1. The boundary already exists — you are paying for it at runtime

`ExecRuntime` (`crates/akuma-exec/src/runtime.rs`) is **44 function pointers**
(46 before the §8.1 merge deleted two), and **13 of them are the PMM**:

```
alloc_page              alloc_page_zeroed        alloc_pages_contiguous_zeroed
free_page               free_pages_contiguous    pmm_stats
free_count              total_count              track_frame
cow_ref_inc             cow_ref_dec              cow_ref_get
is_memory_low
```

That indirection exists for exactly one reason: so `akuma-exec` does not have to
depend on the PMM, which lives in the kernel binary crate. Make the PMM a crate
and the dependency becomes ordinary, the 13 pointers disappear, and three of them
(`alloc_page_zeroed`, `track_frame`, `cow_ref_inc`) are on the **fault path**.

This is the same argument Phase 3 of
[`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
already used once, when it deleted the `NetHal` runtime indirection because it
"cost a spinlocked struct read on the per-packet DMA path to reach two identity
functions." Same shape, larger scale, and this time the indirection also blocks
host testing of the tree's most consequential allocator.

Two of those 13 are already dead weight: `cow_ref_dec` and `cow_ref_get` are **never
called through the runtime table at all** — `cow_ref_inc` (`process/mod.rs:298`) is the
only live CoW pointer. [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §5 found this and
reported "3 of 4"; measured, it was **4 dead of 5**, and the §8.1 merge deleted two of
the four (`cow_fault_lock`, `cow_fault_unlock`), leaving these two for step 5.

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

Net indirection: **−13 (ExecRuntime) +4 (cold PmmHooks) = −9**, and every hot
path — `free_count`, `alloc_page*`, `cow_ref_*` — becomes a direct call.

**What the §8.1 merge already did to this table.** The escalation's *order* and its
give-up decision are no longer in `src/` at all: they are `memmath::next_reclaim_step`
returning a `ReclaimStep`, host-tested, with `src/pmm.rs` holding only a loop that
performs the step it is handed. So this section's work is now narrower and lower-risk
than written — move the four *effects* behind hooks and let the crate call the decision
function it already has. Two notes for whoever does it:

- **The four hooks' `-> usize` return values are not the progress signal.** The loop
  deliberately judges progress by re-reading `free_count()`, because
  `drain_retired_under_pressure` declines *silently* inside its 10 ms cooldown and
  cannot report whether it helped. Keep the return values for diagnostics if you like,
  but a hook that returns 0 must not be treated as "this step is exhausted".
- **`next_reclaim_step` moves with the reserve (§5), not with the effects.** It is pure
  arithmetic over a free-page count; it belongs wherever `USER_PAGE_RESERVE` ends up.

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
`akuma_exec::memmath`. The §8.1 merge then added a third PMM concept for the same
reason — **`next_reclaim_step` / `ReclaimStep`**, the reclaim escalation's decision.
All three are **PMM concepts**; they went to `akuma-exec` because no PMM crate existed
and `src/` was host-unreachable.

They should migrate to `akuma-pmm` when it lands, leaving `memmath` with the fork
copy-range math and the mapping predicates — i.e. the things whose consumer really
is `akuma-exec`. Take their host tests with them: that is 17 tests in `memmath::tests`
today, of which 6 are the escalation's. Treat `memmath` as a correct waypoint under a constraint that
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
- **The escalation** — four recovery actions plus give-up, with a re-check between
  each. Its *decision* is **no longer untested**: the §8.1 merge extracted it as
  `memmath::next_reclaim_step` with 6 host tests, because its own boot test declines
  to exercise it ("actually draining RAM to the reserve is unsafe inside the boot
  suite" — `docs/reference/subsystems/memory.md` → "OOM decision map"). What the four
  injectable hooks add is the *effects*: that each step is actually invoked, in order,
  and only under pressure.

  **Correction to this bullet as originally written.** It said to "register a
  `drain_retired` that returns 0 and assert the escalation does not reach `GiveUp`
  while memory is parked". That is an assertion about a **fix, not about this tree**:
  a fruitless drain is correctly followed by the remaining steps, and after those the
  escalation *does* give up, because no step waits for `PROCESS_RECLAIM_COOLDOWN_US`.
  Writing that test as stated produces a red test, not a caught bug. The behaviour
  that is real, and is now pinned, is split in two:
  `fruitless_drain_retired_continues_instead_of_giving_up` (a fruitless step must not
  short-circuit to `GiveUp`) and
  `give_up_after_the_last_rung_is_the_known_premature_oom` (after the last step it
  does, and that is the open defect). Making the cooldown wait is a behavioural change
  to *when processes get killed* — schedule it as defect work, not as a test.

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

> **The merge LANDED 2026-08-13** (uncommitted at time of writing), so this
> precondition is satisfied. See §8.1's status block for what it changed and the four
> errors it corrected.
>
> **How to check the precondition, if you need to check it again.** Do **not** grep for
> `FaultSlotGuard`: that symbol is §8's *row 1* — the per-page demand-paging slot
> guard — and it landed earlier, in a different change. It is present in trees where
> the CoW merge is absent, so keying the precondition on it returns a false positive
> and walks you straight into the unattributable window this section exists to
> prevent. The reliable checks are:
>
> ```bash
> grep -rn "next_reclaim_step" crates/akuma-exec/src/memmath.rs   # present  => landed
> grep -rn "cow_fault_lock" src/ crates/                          # absent   => landed
> grep -rn "complete_cow_break" src/exceptions.rs                 # 3 call sites
> ```

Suggested order once the merge has landed and been verified:

1. `PmmConfig` + the 4 hooks, still inside `src/` — pure plumbing, no move. Verify.
2. Move the allocator core + `FrameTracker` + ledgers + quarantine into
   `akuma-pmm`, re-exported from `src/pmm.rs` so no call site changes. Verify.
3. Move the CoW refcount table. (`COW_FAULT_LOCK` is already deleted — §8.1 item 2.)
4. Move the escalation's *effects* in; make `free_count` internal. Its decision is
   already `memmath::next_reclaim_step` and travels with the reserve in step 6.
5. Delete the **13** remaining `ExecRuntime` PMM fields and point `akuma-exec` at the
   crate. Verify.
6. Migrate the poison codec, the reserve and `next_reclaim_step` out of `memmath` (§5),
   with their 17 host tests.
7. Host tests: allocator, refcounts, quarantine, and the escalation's *effects* (the
   decision already has 6).

> **Step 7 LANDED 2026-08-14 — the extraction is complete.** 15 new tests in
> `akuma-pmm`:
>
> - **The bitmap allocator (9 tests)** — alloc/free symmetry, exhaustion,
>   `FreeOutcome` (freed/double-free/out-of-range), contiguous-run search, the
>   fragmentation case the plan's §6 named explicitly (enough total free pages,
>   no run long enough), `alloc_pages_into`'s all-or-nothing rollback, and the
>   two-pass word-wraparound search. Each test builds its own **local**
>   `BitmapAllocator` instance rather than touching the crate's global `PMM` —
>   its methods never dereference the "physical" addresses they hand out (pure
>   index arithmetic over the bitmap), so no backing memory or global state is
>   needed, and there is zero cross-test interference under `cargo test`'s
>   default parallelism.
> - **`COW_REFCOUNTS` (5 tests)** — the tree's historically-buggiest
>   accounting (the §5.6 underflow class: one reference per *address space*,
>   not per VA — found in production, fixed three times, never unit-tested
>   before this crate existed) now has: first-share sets 2 not 1, a third
>   owner adds one more, dec-to-zero removes the entry and reports the last
>   owner, a never-shared PA decs safely as a single owner, and the count
>   matches address-space count rather than per-VA mapping count. These share
>   the crate's one global `COW_REFCOUNTS` map — safe because each test picks
>   its own PA, never reused by another test, so concurrent access can only
>   interleave at the map's spinlock, never corrupt a result.
> - **Quarantine + escalation effects (1 test, deliberately not 2)** — needs a
>   real backing arena (`poison_page` writes through `phys_to_virt`) and the
>   crate's actual global `PMM` bitmap/`ALLOCATED_PAGES`/hooks, so it got its
>   own `test_arena::ensure_pmm()` (mirrors `akuma-exec`'s
>   `test_support::ensure_test_pmm`, but crate-local — a different crate's test
>   binary is a different process with its own copy of every static here).
>   Covers: a UAF write to a quarantined frame is detected on release: a
>   double-free of a still-quarantined frame is caught by `QUAR_PRESENT`; and
>   `alloc_page_zeroed_user` walks all four `PmmHooks` in the documented
>   cheapest-first order, exactly once each, before giving up, verified via
>   instrumented hook functions that stamp a shared call-order sequence.
>   Folded into one `#[test]` fn rather than two: the escalation half drives
>   free pages down to the reserve by allocating nearly the whole arena, and a
>   concurrently-running sibling test touching the same global bitmap could
>   interleave an alloc/free in between the escalation loop's `free_count()`
>   re-checks and flip which rung it takes — the exact "measurement, not the
>   code" failure class this runbook's own doc comment was written after.
>
> Verified: all 4 build profiles clean, 506 host tests passing (491 + 15,
> matching exactly), 28/28 in `akuma-pmm` stable across 5 repeated runs, SMP=1
> and SMP=4 boot clean with all 5 exercises passing.

> **Steps 1–5 LANDED 2026-08-14.** Two corrections to what's written above:
>
> - **Step 4's escalation loop needed a decision function `akuma-pmm` cannot
>   reach yet** (`ReclaimStep`/`next_reclaim_step` are still `memmath`'s — they
>   don't move until step 6, per §5 below). Resolved with the same
>   temporary-duplicate pattern §5 already uses for `poison_word`: a private
>   `reclaim_escalation` module inside the crate, host-tested identically to
>   `memmath::tests`' 6 escalation tests, deleted the moment step 6 lands the
>   real one.
> - **§1's count of 13 was one over.** `is_memory_low` is PMM-shaped (it mostly
>   reads `pmm::free_count()`) but its implementation, `allocator::is_memory_low`,
>   lives in `src/` and falls back to the kernel heap's own byte accounting
>   before the PMM is up (`is_pmm_ready()`) — state `akuma_pmm` has no way to
>   reach, and dropping that fallback to make the field disappear would be a
>   real behaviour change this step must not make. It stays an `ExecRuntime`
>   hook, correctly: unlike the other 12, it was never leftover-from-when-PMM-
>   lived-in-`src/` indirection, it's the ordinary hook-down-into-`src/`
>   direction `ExecRuntime` exists for. So step 5 deleted 12 fields, not 13.
>
> Also found, worth recording since the plan's §1 inventory didn't catch it:
> `free_count`, `total_count`, `cow_ref_dec` and `cow_ref_get` (not just
> `cow_ref_dec`/`cow_ref_get`, as §1 said — "2 of those 13") were **already
> dead weight inside `akuma-exec` itself** before this step — registered in
> `ExecRuntime` and faked in `test_support.rs`, but never once read via
> `runtime()` anywhere in the crate's real logic. `src/`'s own call sites
> (`exceptions.rs`, `allocator.rs`, `tests.rs`, …) always called
> `crate::pmm::free_count()` etc. directly. Deleting the fields cost nothing at
> those four.
>
> Deleting the 12 also deleted `ensure_test_runtime`'s dozen PMM fakes
> (`alloc_page_zeroed: || None` and friends) — every `akuma-exec` host test that
> reaches a (now direct) `akuma_pmm::*` call site needed a REAL PMM behind it
> instead. This is §6's promised payoff arriving early: `test_support.rs` now
> has `ensure_test_pmm()`, which runs the real allocator over a 64 MiB leaked
> host arena (`akuma_primitives::phys_to_virt` is the identity, so a real host
> address works as a "physical" page directly) — strictly better coverage than
> the old always-`None` fake, and it caught nothing broken: all 239
> `akuma-exec` host tests and all 499 workspace host tests passed unchanged.
> Quarantine and the premature-free check stay off in that test PMM, since
> turning them on is a real behaviour change to `free_page` timing that belongs
> to step 7 ("host tests: … quarantine"), not this one.

> **Step 6 LANDED 2026-08-14.** The reserve, the escalation's decision, and the
> poison codec's pure half all moved into `akuma-pmm` for real, deleting the
> Step 4 temporary duplicates. One piece did **not** follow, and the plan as
> written didn't anticipate it: `poison_word_frame`, the gated wrapper that
> supplies the *live* `mmu::ram_base()`/`ram_end()` window and reads
> `config().pmm_uaf_quarantine`, needs `akuma-exec` state (`mmu`) this crate
> structurally cannot reach. It moved to `src/pmm.rs` instead — the same
> resting place `report_poison_value` (its one caller) already had, for the
> identical reason. Its config gate switched from `ExecConfig`'s own
> `pmm_uaf_quarantine` copy to `akuma_pmm::PmmConfig`'s (both were always fed
> from the same `config::PMM_UAF_QUARANTINE` kernel constant, so this is
> behaviour-preserving), and `ExecConfig::pmm_uaf_quarantine` — now read
> nowhere in `akuma-exec` — was deleted as dead weight, the same class of
> finding as step 5's four dead `ExecRuntime` fields.
>
> One test did not migrate: `gated_decode_uses_the_live_ram_window`, which
> exercised `poison_word_frame` itself (not the pure `poison_decode` it wraps,
> which kept its 4 tests). That coverage has no host-reachable home any more —
> `poison_word_frame` lives in `src/pmm.rs`, and `src/` cannot be host-tested —
> the same gap `report_poison_value` already had before this. So: 8 of the
> plan's cited "17 host tests" is the right number to have expected moving in
> Step 6 (the reserve's 3 + the poison codec's 4 portable ones + the one that
> didn't survive = the 8 non-escalation, non-mapping-predicate tests memmath
> had), not 17 — the escalation's 6 already moved in Step 4, and the mapping
> predicates' 3 were never PMM's to move. `memmath.rs` is now 116 lines, holding
> only `mapping_is_read_only_to_user`/`is_shareable_mapping` and their 3 tests.
> Verified: all 4 build profiles clean, 491 host tests passing (down from 499 —
> exactly the 8 net tests this step's bookkeeping predicts), SMP=1 and SMP=4
> boot clean with all 5 exercises passing.

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

> **Corrected 2026-08-14 — the milestone was never actually red; the measurement
> was.** `bssfork`'s CLI is positional (`bssfork [rounds] [threads] [spread]`), not
> `key=value` (see the binary's own usage comment, `bssfork.c:39`). Every
> measurement below that ran the literal command `bssfork spread=1` fed the string
> `"spread=1"` into `rounds`; `strtoul` parses that as `0`, and `spread` silently
> defaulted to `0` too. With `rounds=0` the fork loop never executes, so the main
> thread sets `g_stop=1` almost immediately after creating the workers, and the
> liveness check flags threads `[never ran]` before the scheduler gets to them —
> a race in the *test's own setup*, reproducible on **any** kernel, with nothing to
> do with CoW, frame allocation, or thread admission.
>
> Re-run 2026-08-14 at SMP=4 on HEAD (`284d1d0`), both invocations, to confirm:
> the broken form (`bssfork spread=1`) reproduced `failures=8, ticks=0` on 2 of 3
> runs (and `failures=6`/`7` the others) — matching the shape recorded below almost
> exactly — while the correct form (`bssfork 20 8 1`, real `rounds=20 threads=8
> spread=1`) passed **8/8 clean runs**, `failures=0` every time. So: criterion 3
> below is met outright (`spread=1` reaches `failures=0`), criterion 2 dissolves
> (the "8/8-vs-7/8 gap between main and the branch" was two independent
> mis-invocations landing on nearby random counts, not a regression), and criterion
> 4 is moot — there is no diagnosis to cover with a host test. `scripts/verify_trim.py`
> now runs both `bssfork` and `bssfork 20 8 1` as separate exercises (`ex.bssfork`
> and `ex.bssfork_spread1`) so a real regression in either shape is still caught.
> The extraction can proceed without this as an open question. The rest of this
> section is kept verbatim as the investigation record.

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
