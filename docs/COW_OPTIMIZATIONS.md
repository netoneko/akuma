# `fork` / Copy-on-Write — Why It's Slow, and How to Fix It

Bringing up `rustc` (see `docs/RUST_TOOLCHAIN.md`) made the `fork` subsystem
*correct* for multithreaded processes but exposed how *expensive* it is: an
end-to-end `rustc hello.rs` takes ~3 minutes, almost all of it kernel time in
`fork`/`mmap`/`munmap`. This document explains where the time goes and the
options for fixing it. Nothing here is a correctness bug — it is all performance.

Code: `crates/akuma-exec/src/process/mod.rs` (`fork_process`), `…/mmu/mod.rs`
(address space, `track_user_frame`/`remove_user_frame`/`unmap_page`),
`src/syscall/mem.rs` (`sys_mmap`/`sys_munmap`).

---

## TL;DR

1. **The working set grew ~15×.** The multithreaded-`fork` correctness fix
   (`RUST_TOOLCHAIN.md` §4b′) makes `fork` replicate the *whole thread group's*
   address space. For rustc that jumped from ~4,800 pages to **~75,000 pages**
   shared per `fork` (`[FORK-COW] shared 75049 pages in 50584µs`).
2. **`munmap` is accidentally O(n²).** `remove_user_frame` is a linear `Vec`
   scan, called once per page. Unmapping a large region is therefore
   `O(pages × tracked_frames)`. With the tracked-frame list now ~15× longer, a
   single 12,426-page `munmap` costs ~190 ms; the run does ~150 of them.
3. **Everything is per-page.** Sharing, demoting, unmapping, and TLB flushing
   all loop a page at a time across the entire address space.
4. **`fork`+`exec` throws all of it away.** The spawned child `exec`s
   immediately, so `replace_image` discards the entire freshly-replicated
   address space. For the common spawn pattern, **100 % of the CoW work is
   wasted**.

The single highest-leverage fixes: make frame teardown not be O(n²) (quick), and
add a vfork fast-path so `fork`+`exec` doesn't copy at all (structural).

---

## How `fork`'s CoW works today

`fork_process` (with `cow_fork_enabled`) builds the child address space by
**enumerating tracked regions** and sharing them page-by-page:

```
for each region in { stack, code+brk, interp window, parent.mmap_regions,
                     sibling threads' mmap_regions, tgid lazy regions }:
    cow_share_range(parent_l0, va, len, child_as)
```

`cow_share_range` (mod.rs ~1351):

```rust
let mapped = mmu::collect_mapped_pages_with_flags(parent_l0, va_start, pages); // walk PT, per page
for (va, pa, pte_flags) in mapped {
    (runtime().cow_ref_inc)(pa);              // bump CoW refcount
    child_as.map_page(va, pa, RO_flags)?;     // map into child RO (may alloc L1/L2/L3 frames)
    child_as.track_user_frame(PhysFrame::new(pa)); // push onto child's user_frames Vec
}
```

Then the parent side is demoted to RO over the same ranges
(`demote_range_to_ro`, per page) and the TLB is flushed (`flush_tlb_asid(0)` —
**all** ASIDs). Later, a write on either side takes a CoW fault that allocates a
private copy.

So per `fork` the kernel does, for **every mapped page in the whole address
space**: a page-table walk to collect it, a refcount increment, a child
`map_page`, a `track_user_frame` push, and a parent demote — plus a scan over the
*unmapped* gaps in each region's VA span.

---

## Why it is slow *now* (it wasn't before — it was just broken)

Before §4b′, `fork` only copied the **forking thread's own** regions. For a
multithreaded process that was a small slice of the address space — fast, but
*incorrect*: the child was missing its siblings' stacks and SIGSEGV'd (that was
the whole rustc bug). The fix made `fork` faithful, which means it now touches
the entire ~300 MB / ~75k-page working set. The cost was always O(address-space
size); the fix simply made `fork` actually pay it.

### 1. O(n²) teardown via `remove_user_frame` — the dominant cost

`mmu/mod.rs`:

```rust
pub fn remove_user_frame(&self, frame: PhysFrame) {
    let mut frames = self.user_frames.lock();
    if let Some(idx) = frames.iter().position(|f| f.addr == frame.addr) { // O(n) scan
        frames.swap_remove(idx);
    }
}
```

`sys_munmap` calls this **once per page** of the region being freed, and
`unmap_page` does a **per-page TLB flush**. Unmapping `P` pages from an address
space tracking `n` frames is therefore `O(P · n)`. The logs show single
`munmap`s of 12,426 pages (`[munmap] pid=70 … full (12426 pages)`); with `n ≈
75,000` that is ~9×10⁸ comparisons for one call. PSTATS:
`munmap=148(29500ms)` and `mmap=158(30446ms)` — i.e. ~60 s, ~190 ms/call,
dominating the ~64 s of in-kernel time. **The §4b′ fix multiplied `n` ~15×, so
these large unmaps got ~15× slower.**

### 2. The `fork`+`exec` waste

Every spawn is `fork` → (child) `exec`. `replace_image` tears down the entire
address space the child just inherited and loads the new binary. So for the
spawn path — which is *all* rustc does (it forks to run `clang`, which forks to
run `ld`) — the full CoW share, the RO demotion, the CoW faults, and the
teardown are **pure overhead**. Real `vfork`/`posix_spawn` exists precisely to
avoid this; Akuma routes `CLONE_VFORK|CLONE_VM` to the same full-copy
`fork_process` and merely blocks the parent.

### 3. Per-page everything

`collect_mapped_pages_with_flags`, `map_page`, `demote_range_to_ro`, and
`unmap_page` all iterate one 4 KB page at a time, and `unmap_page` flushes the
TLB per page. There is no sharing of page-table *subtrees* (an L2 entry covers
512 pages / 2 MB; an L1 entry covers 1 GB) and no batched TLB maintenance.

### 4. Scanning unmapped gaps

`cow_share_range` scans the full VA span of each region (e.g. the 2 MB interp
window, the `code_start..brk` span) page-by-page even where nothing is mapped,
and `demote_range_to_ro` re-walks the same spans. Wasted walks scale with the
*address-space layout span*, not just the resident set.

### 5. Multiple expensive forks per compile

`rustc` → `clang-21` → `ld` is at least two `fork_process` calls, each
replicating a large multithreaded address space (`shared 75049` and `67665`
pages in the trace), and each followed by large teardown.

### Measurements (rustc6.log, 2026-05-31)

| Quantity | Value |
|---|---|
| Pages shared per `fork` | 75,049 (was ~4,804 pre-§4b′) |
| `fork` CoW share time | ~50 ms/fork |
| `mmap` syscalls / time | 158 / 30,446 ms |
| `munmap` syscalls / time | 148 / 29,500 ms |
| Largest single `munmap` | 12,426 pages |
| In-kernel time (one compile) | ~64 s |
| Wall-clock (one compile) | ~3 min |

The per-`fork` share (~50 ms) is *not* the headline; the headline is the
~60 s spent in large `mmap`/`munmap` whose per-page cost is amplified by the
O(n) frame bookkeeping over a now-much-larger tracked-frame list.

---

## Optimization options (ranked by leverage / cost)

### A. Make frame teardown not O(n²) — *quick, biggest immediate win*

`user_frames` should not be a `Vec` scanned per page. Options:
- Replace the linear scan with a `BTreeSet<usize>`/hash set keyed by PA → O(log n)
  / O(1) removal; or
- Drop per-frame tracking entirely and treat the **page tables as the source of
  truth** — `munmap`/teardown walks the L3 entries for the range and frees what
  it finds (with CoW-refcount-aware freeing), eliminating the parallel `Vec`.

This alone should turn the ~190 ms unmaps into single-digit ms and is the
lowest-risk change. It also helps every process, not just `fork`.

### B. `vfork` fast-path for `fork`+`exec` — *structural, biggest absolute win*

For `CLONE_VFORK|CLONE_VM` (what `posix_spawn`/libstd's spawn uses), don't copy
the address space at all:
- Share the parent's page tables with the child (already have `new_shared`),
- Suspend the parent thread until the child `exec`s or `_exit`s (already done for
  vfork),
- On `exec`, the child builds a fresh address space anyway — so the shared one is
  simply dropped, never copied or demoted.

This removes the entire replicate-then-discard cycle for the spawn path, which is
the *only* path rustc exercises. Care: while shared, the child must not write
parent memory before `exec` (the vfork contract — libstd respects it).

### C. Coarse-grained CoW — share page-table subtrees, not pages

Instead of per-page `map_page` + refcount, share whole L1/L2 tables by bumping a
refcount on the **table frame** and marking the entries read-only. A fork then
costs `O(number of tables)` (hundreds) instead of `O(number of pages)`
(tens of thousands). On a write fault, split the shared subtree lazily. This is
the standard kernel approach and makes `fork` cost independent of resident size.

### D. Lazy child population

Don't eagerly `map_page` every page into the child. Mark the shared regions
copy-on-write in the parent and let the child fault them in on first access
(Akuma already has demand-paged "lazy regions" — extend that to forked pages).
Combines well with B/C.

### E. Cheap wins

- Per-ASID TLB flush instead of `flush_tlb_asid(0)` (all ASIDs) after fork.
- Batch TLB maintenance in `unmap_page` (one barrier per range, not per page).
- Skip unmapped gaps: drive `cow_share_range`/demote from the page-table walk's
  *present* entries rather than re-scanning whole VA spans.

---

## Recommended path

1. **A first** (frame teardown O(1)) — small, safe, kills the dominant ~60 s
   and helps all process exit/`munmap`, not just fork.
2. **B next** (vfork fast-path) — eliminates the wasted copy for every spawn,
   which is the entire rustc/clang/ld pipeline.
3. **C/D later** if `fork` of a *non-exec* child (rare here) still matters.

A + B together should take a `rustc` compile from minutes to seconds without
touching the §4b′ correctness guarantees. All of these change a critical path
(a bug breaks *all* process spawning), so each should land with the existing
`fork`/pipe boot self-tests green and an end-to-end `rustc hello.rs` re-run.

---

## References

- `fork_process`, `cow_share_range`, `demote_range_to_ro`:
  `crates/akuma-exec/src/process/mod.rs`
- `track_user_frame` / `remove_user_frame` (O(n) scan) / `unmap_page`
  (per-page TLB flush) / `user_frames`: `crates/akuma-exec/src/mmu/mod.rs`
- `sys_mmap` / `sys_munmap`: `src/syscall/mem.rs`
- Correctness fix that exposed this (multithreaded-`fork` address-space
  replication) and the end-to-end result: `docs/RUST_TOOLCHAIN.md` §4b′, §5b
