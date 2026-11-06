# CoW break, trampoline and single-page fill staged through `FreshFrame` (2026-11-06)

A change to a path with a long history (`COW_PILE_AUDIT.md`, `COWSTALE_FORK_THREAD_SEGV.md`),
written down so that **if a CoW, fork or SIGSEGV symptom shows up later, this is the first diff to
bisect against.** Commits: `da9a90a3` (the conversion) and the follow-up that restored free
attribution (see "What did not change").

## What changed

Three aarch64 sites in `crates/akuma-exceptions` held a bare `PhysFrame` (a `Copy` address, no
ownership) and wrote into it through `phys_to_virt` + raw pointers. They now hold an
`akuma_phys::FreshFrame` while the page is staged and only give it up when it is mapped:

| site | before | after |
|---|---|---|
| `complete_cow_break` (3 callers) | `new_frame: PhysFrame`; `copy_nonoverlapping` | `new_frame: FreshFrame`; `copy_from_frame`; `publish()` after the PTE is installed and the frame tracked |
| `ensure_sigreturn_trampoline` | raw `copy_nonoverlapping` into the page | `fill_with`, then `publish()` into a `PhysFrame` for the existing map-or-free code |
| single-page demand fill (`readahead pool exhausted` arm) | `from_raw_parts_mut` slice for `read_at` | `fill_with(dst_off, len, ..)` |

New `FreshFrame` API: `alloc_zeroed_clean` (the PMM's own `alloc_page_zeroed`, which zeroes **and
cleans the data cache** - `FreshFrame::alloc_zeroed` does not and must not be used for AArch64 user
pages), `alloc_zeroed_user` (the reserve-respecting, reclaim-escalating variant), `fill_with`
(bounds-checked) and `free_by(tid)`. 3 `unsafe` sites retired (budget 472 -> 469).

## The one behavior change: a frame the old code leaked

The EL0 write-permission-fault arm allocates the CoW destination **before** taking `as_lock` and
returns it when the page turns out not to need a break. The old code had two exits after a failed
break:

```rust
if did_cow { return frame.x0(); }
...
if stale_write_fault_absorbed(page_va) {
    return frame.x0();        // <- new_frame was still live here: LEAKED
}
free_page(new_frame);         // only the fall-through path freed it
```

When `stale_write_fault_absorbed` is true (a sibling thread repaired the page while this one waited
on the per-page fault slot - the residual `cowstale` shape), the pre-allocated, zeroed, never-mapped,
never-tracked frame was dropped on the floor: one 4 KiB frame per absorbed fault, unrecoverable. With
`FreshFrame` the frame is freed by `Drop` on that return, because the type owns it until
`complete_cow_break` consumes it.

**Why this is believed safe:** the frame is exactly the kind `free_page` already receives from the
adjacent fall-through exit - allocated from the PMM, zeroed, absent from every page table, `user_frames`
and `COW_REFCOUNTS` - so `free_page` takes the ordinary untracked path (`cow_ref_dec` finds no entry,
`untrack_frame` is a no-op, then the quarantine / free list). It runs while `_cow_fault_guard` is still
held, as the explicit free did on the other exit. **Not shown by test:** neither the gate nor the boot
tests drive the absorbed path deterministically; `cowstale` reaches it probabilistically (~40 % per run
at SMP=2 before `stale_write_fault_absorbed` existed).

**If this breaks later, the signature would be:** a double free or `[DOUBLE-FREE]` / premature-free
report naming a frame allocated by the EL0 CoW arm; or `ALLOCATED_PAGES` going *below* what the free list
implies after a `cowstale` or fork-hammer run. The leak this fixes would instead have shown as free
pages drifting down by one per absorbed fault.

## What did not change

* Declined `TakingAsLock` break (F1b): frees with the real `current_tid`, via `free_by`. Same
  `free_page(pa, tid)` call as before.
* The two "no owner" exits in `ensure_cow_page_writable` / `try_resolve_el1_cow_fault`: first
  converted to a bare `drop` (tid 0 in the free ledger and premature-free reports); restored to
  `free_by(current_tid)` the same day so attribution is as before. `Drop` still frees with tid 0, so
  a *new* exit that forgets `free_by` loses the thread id - that is diagnostics only.
* Ordering inside the break: copy under `as_lock`, `map_page`, TLB flush, `track_user_frame`, then
  publish. `publish` happens after the closure in the `TakingAsLock` arm, not inside it, so a decline
  frees outside the `as_lock` hold exactly as before.

## Verification

* `cargo check --release`, clippy on `akuma-phys` + `akuma-exceptions` (`aarch64-unknown-none`), unsafe
  budget (469).
* `SMP=4 sh shanghai/mw_gate.sh all`: PASS on aarch64 HVF and amd64 TCG.
* Boot tests: `cow_break_declines_stale_old_pa`, `cow_break_dec_only_on_last_va`,
  `cow_break_on_shared_view_leaks_both_frames` PASSED.
* CoW probes (`scripts/mem_suite.py --only cowstale,madvshared,...` and the fork hammer): see the
  result section below.

## Probe results

(filled in after the probe run)

## Open

* The absorbed-path free is not deterministically tested. A probe that forces `stale_write_fault_absorbed`
  and asserts the PMM free count is unchanged would pin it.
* The batch `frame_pool` demand-fill loop (`alloc_pages_zeroed`) still carries bare `PhysFrame`s and
  leaks the unconsumed tail on an early return; it is the next `FreshFrame` conversion
  (`UNSAFE_OWNERSHIP_TYPES_DESIGN.md` 2.1).

## Background

`UNSAFE_OWNERSHIP_TYPES_DESIGN.md` (the design this lands step 2 of), `COW_PILE_AUDIT.md` (F1, F1b, F2),
`COWSTALE_FORK_THREAD_SEGV.md` (`stale_write_fault_absorbed`, the `cowstale` probe).
