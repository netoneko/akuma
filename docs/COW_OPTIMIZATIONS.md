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

### Kernel benchmark (boot self-test)

`src/process_tests.rs::run_cow_benchmarks()` measures the two costs directly at
boot and prints grep-able `[BENCH]` lines.  It allocates real frames and is
memory-adaptive (capped by free RAM with headroom), so it is safe at the default
256M; boot with `MEMORY=2048` to reach the larger working-set size uncapped.

- **BENCH-1 `munmap-teardown`** — maps+tracks `n` pages, then tears them all
  down via the exact `munmap` primitives (`unmap_and_free_page` →
  `remove_user_frame` + per-page TLB flush + `free_page`).  Run at `n=2000` and
  `n=16000`.  The headline is `per_page`: under the O(n²) teardown it *grows*
  with `n`; once teardown is O(log n) it is *flat*.
- **BENCH-2 `fork-cow-share`** — runs the per-page primitives
  `fork_process`'s `cow_share_range` uses (`collect_mapped_pages_with_flags` →
  `cow_ref_inc` + child `map_page` + `track_user_frame`) plus the parent
  `demote_range_to_ro` + TLB flush, over 8000 pages.  Informational (targets
  C/D/E) and a guard that Fix A doesn't regress the fork path.

| Benchmark (`MEMORY=2048`) | Baseline | After Fix A | After Phase 2 (E) |
|---|---|---|---|
| `munmap-teardown` n=2000, per page | 1,902 ns | 898 ns | 828 ns |
| `munmap-teardown` n=16000, per page | **12,220 ns** | 843 ns | **664 ns** |
| `munmap-teardown` n=16000, total | 195,531 µs | 13,494 µs | **10,636 µs** |
| `fork-cow-share` 8000 pages, per page | 576 ns | 792 ns | ~800 ns |

Baseline per-page cost scales with `n` (1,902 → 12,220 ns as n goes 8×) — the
O(n²) signature, and the 16k total (~195 ms) matches the ~190 ms `munmap`s seen
in the rustc trace.  After Fix A, per-page teardown is **flat in `n`** (898 vs
843 ns) — a **14.5× speedup** on the large unmap.  After Phase 2 (batched TLB
flush + full-flush threshold) the 16k unmap is **664 ns/page** — **18.4×** vs the
original baseline.  `fork-cow-share` rose with Fix A (576 → ~800 ns/page) because
`track_user_frame` went from `Vec::push` (O(1)) to a map insert (O(log n)); this
is dwarfed by the teardown win and is what C/D address next.

> **QEMU caveat:** these are TCG-emulated numbers.  TLB-maintenance
> instructions (`tlbi`) and barriers (`dsb`/`isb`) are far cheaper under
> emulation than on real AArch64 silicon, so the Phase 2 TLB wins — especially
> the full-flush threshold — understate the real-hardware benefit.

### End-to-end `rustc hello.rs` (Fix A + E + B, 2026-05-31)

`rustc -C linker=clang /akuma-playground/hello.rs` on the VM (`MEMORY=2048`),
PSTATS from the rustc/clang/ld processes:

| Quantity | Baseline (rustc6.log) | After A+E+B |
|---|---|---|
| Wall-clock (one compile) | ~180 s | **~118 s** |
| `fork` CoW share (per spawn) | 75,049 pages, ~50 ms | **0 for rustc spawns** (shared-AS vfork) |
| rustc `clone` total | (full copy each) | **7 clones / 78 ms** (~11 ms ea) |
| rustc `munmap` | 148 / 29,500 ms | **209 / 88 ms** |
| rustc `mmap` | 158 / 30,446 ms | 216 / 27,138 ms |

`fork`/`munmap` are no longer the bottleneck: Fix B removed the per-spawn CoW
replication (zero `FORK-COW` shares from rustc), and Fix A+E cut `munmap` from
~199 ms/call to ~0.4 ms/call (29.5 s → 88 ms total). Memory returns to the
pre-compile baseline (no leak from the shared-AS path) and all 196 boot
self-tests + 98 host tests stay green.

**The remaining cost is now `mmap`** (~27 s, eager page allocation + zeroing),
a separate axis from fork/CoW — the next thing to attack if compile time still
matters (e.g. lazy/zero-on-demand population, huge-page zeroing).

---

## Optimization options (ranked by leverage / cost)

### A. Make frame teardown not O(n²) — *quick, biggest immediate win* ✅ DONE

`user_frames` should not be a `Vec` scanned per page. Options:
- Replace the linear scan with a `BTreeSet<usize>`/hash set keyed by PA → O(log n)
  / O(1) removal; or
- Drop per-frame tracking entirely and treat the **page tables as the source of
  truth** — `munmap`/teardown walks the L3 entries for the range and frees what
  it finds (with CoW-refcount-aware freeing), eliminating the parallel `Vec`.

This alone should turn the ~190 ms unmaps into single-digit ms and is the
lowest-risk change. It also helps every process, not just `fork`.

**Implemented** (`mmu/mod.rs`): `user_frames` is now a
`BTreeMap<usize, u32>` (PA → in-AS refcount), so `remove_user_frame` is
O(log n) instead of an O(n) scan.  A *map with a count* (not a plain set)
preserves exact free multiplicity if a PA is ever tracked at multiple VAs, so
the CoW-refcounted free path stays balanced (no leak / no double-free).
Result: the 16k-page teardown went **195 ms → 13.5 ms (14.5×)** and per-page
cost is now flat in `n` (see the benchmark table above).  Boot fork/clone/pipe
self-tests stay green and host unit tests pass.

### B. `vfork` fast-path for `fork`+`exec` — *structural, biggest absolute win* ✅ DONE

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

**Implemented** as `process::vfork_process` (gated by
`config::VFORK_FASTPATH_ENABLED`, default on). `sys_clone` routes a
`CLONE_VFORK` child to it: the child gets a `new_shared(parent_l0)` address
space (no CoW share, no demote, no parent TLB flush), runs on its caller-provided
stack, and on `execve` `replace_image` drops the shared view (refcount--),
leaving the parent's pages untouched — so the **parent takes zero CoW faults**
when it resumes. Because the shared L0 already contains every sibling thread's
stack, the fast-path also satisfies the §4b′ multithreaded-fork correctness
guarantee *for free* (no replication needed).

Identity: a shared-AS child reads the parent's `PROCESS_INFO` page, so
`read_current_pid` now resolves via `THREAD_PID_MAP → tgid` when the flag is on.
This is behavior-preserving for normal threads (`getpid` still returns the tgid)
and gives the vfork child its own pid. **Verified end-to-end:**
`rustc -C linker=clang hello.rs` compiles → links (`clang-21`→`ld`) → runs
(`Hello from Akuma!`), with **zero** `FORK-COW` shares attributable to rustc
(all 7 rustc `clone`s + clang's `clone` took the shared-AS path).

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

- **Batch TLB maintenance in `munmap`** ✅ **Done.** Added `unmap_page_no_flush`
  / `unmap_and_free_page_no_flush` and `flush_tlb_range_all_asid`; `sys_munmap`
  now clears all PTEs in a region with no per-page barrier, then flushes the
  region once.  Above a 512-page threshold it does a single full-TLB flush
  (`tlbi vmalle1`) instead of one `tlbi vaae1` per page — like Linux's
  `tlb_single_page_flush_ceiling`.  16k-page teardown: 843 → 664 ns/page (the
  gain is larger on real hardware; see the QEMU caveat above).
- **Skip unmapped gaps** ✅ **Already done.**  `collect_mapped_pages_with_flags`
  and `demote_range_to_ro` both already skip absent L0/L1/L2 subtrees by block
  boundary, so the fork share/demote walks only touch present entries.
- Per-ASID TLB flush instead of `flush_tlb_asid(0)` after fork — **skipped on
  purpose.**  `flush_tlb_asid(0)` is `tlbi aside1` with ASID 0, and sibling
  threads of a multithreaded process each hold a *different* ASID over the same
  L0; a naive per-ASID flush would leave stale RW entries in siblings' TLBs and
  reintroduce the §4b′ multithreaded-fork corruption.  It is also just one flush
  per fork (~negligible).  Not worth the risk; left as-is.

---

## Recommended path

1. **A first** (frame teardown O(1)) — small, safe, kills the dominant ~60 s
   and helps all process exit/`munmap`, not just fork. ✅ **Done** — 14.5× on
   the large unmap; per-page teardown now flat in `n` (see benchmark table).
2. **B next** (vfork fast-path) — eliminates the wasted copy for every spawn,
   which is the entire rustc/clang/ld pipeline. ✅ **Done** — see end-to-end
   results below.
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
