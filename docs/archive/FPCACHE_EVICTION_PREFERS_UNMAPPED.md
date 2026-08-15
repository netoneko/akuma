# The file-page cache evicted hot pages when full (2026-08-15)

> **Status: FIXED.** `file_page_cache::insert`'s over-cap eviction took whatever entry
> sat at the rotating cursor, mapped or not. Evicting a *mapped* entry frees nothing
> and costs the next mapper a `read_at`, so a full cache re-read its own hot working
> set from disk. It now prefers an unmapped entry, via the same test `shrink` already
> used, and `evict_mapped=` makes the remainder visible.
>
> **It is not a fix for a slowdown** — see §4, where the slowdown that led here turned
> out not to exist.

## 1. The bug

`src/file_page_cache.rs` has two eviction paths, and only one of them was careful.

`shrink(want)` — the memory-pressure path — deliberately takes only entries nobody
maps, because those are the only ones whose eviction actually returns memory:

```rust
// refcount 1 == cached with no mappers: freeing it actually returns memory.
.filter(|(_, e)| crate::pmm::cow_ref_get(e.pa) <= 1)
```

`insert()` — the over-cap path — did not:

```rust
let cursor = EVICT_CURSOR.load(Ordering::Relaxed);
let victim = pages.range((0u32, cursor)..)
    .find(|(k, _)| *k != (inode, file_off))      // whatever is here, mapped or not
```

Evicting a **mapped** entry is the worst of both:

- it frees **nothing** — the frame survives on its mappers' references, exactly as the
  module's refcount invariant says it should;
- it costs the **next** mapper of that page a full `read_at`, because the dedup entry
  that would have turned that fault into a hit is gone.

And this is the path a *full* cache takes on **every** insert. The cap is
`RAM/8` pages (131,072 on a 4 GB box), and the self-host build sits pinned at it:
`entries=130957` against a cap of 131072, in every arm measured.

That is the same self-reinforcing shape the module doc describes as the reason the
cache exists at all — "more pressure → more eviction → more I/O" — reintroduced by the
cache's own eviction policy once it fills.

## 2. The fix

Prefer an unmapped victim, using the identical `cow_ref_get(pa) <= 1` test `shrink`
already uses, over a **bounded** scan from the rotating cursor:

```rust
const EVICT_SCAN: usize = 64;
for (k, v) in pages.range((0u32, cursor)..)
        .chain(pages.range(..(0u32, cursor)))     // wrap, each entry seen once
        .filter(|(k, _)| *k != (inode, file_off))
        .take(EVICT_SCAN)
{
    if fallback.is_none() { fallback = Some((k, v)); }
    if crate::pmm::cow_ref_get(v.pa) <= 1 { unmapped = Some((k, v)); break; }
}
let victim = unmapped.or(fallback);
```

Three things the shape is load-bearing on:

- **Bounded.** `cow_ref_get` takes the CoW table lock per candidate, and this runs
  inside the `PAGES` hold with IRQs masked. An unbounded scan would put a lock
  acquisition per cached entry inside an IRQ-masked critical section.
- **Falls back rather than growing.** If the window turns up no unmapped entry, it
  evicts the cursor entry anyway. Refusing to evict would let the cache exceed its cap,
  which is a worse failure than one expensive eviction.
- **Lock order is unchanged.** `COW_REFCOUNTS` is a leaf and is taken innermost;
  `shrink` already establishes `PAGES` → `COW_REFCOUNTS`.

## 3. Measuring it: `evict_mapped`

`evict` alone cannot distinguish a cheap eviction from an expensive one, which is why
the policy bug survived — the counter that was there could not see it. `EVICTIONS_MAPPED`
counts the evictions that still had to take a mapped entry, and rides in the same line:

```
[FPCACHE] entries=127483 hits=6458722 misses=170799 evict=2159 evict_mapped=834 inval=18278
```

**Read the ratio, not the total.** `evict_mapped / evict` = 39% here, which says the
bounded scan often finds *no* unmapped entry — i.e. the working set genuinely exceeds
the cap, and the cache is too small for this workload rather than merely full. That is
a sizing question ([`FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](FPCACHE_UNDERSIZED_AT_LOW_RAM.md)),
and this fix does not address it; it only stops the cache making the situation worse
by choosing badly among the victims it has.

**Honest limit on the claim:** `evict_mapped` did not exist before the fix, so there is
no before/after number for it. What can be said is that the old policy applied no
preference at all, and that the new one is correct by construction. No speed claim is
made — see §4.

## 4. What led here, and why the premise was wrong

This was opened to explain a suspected 2× build slowdown. **That slowdown does not
exist.** Build wall time on the self-host workload, `cargo clean` before each run:

| arm | wall times (s) | mean |
|---|---|---|
| all fixes applied | 153, 119, 93, 106, 114 | ~117 |
| **`7e379b17` — unmodified branch** | 147, 109, 98, 116, 42, 99 | ~102 |
| two-walk `munmap` arm | 162, 98, 97, 102, 97, 197, 107, 93, 104, 109 | ~117 |
| the arm the "slowdown" was measured against | 44, 44, 43, 44, 43, 44 | **44** |

Every arm sits in the same band as the **unmodified** branch. The 44 s arm is the
outlier, and its *flatness* is the tell: every other arm — including code with no
changes at all — swings between 42 s and 197 s, while that one sat at 43-44 s six times
running with ±1 s. Identical work each round (same two crates failing) at a quarter of
the time and a twentieth of the variance is a different workload, not a faster kernel;
the likely cause is a `cargo clean` that did not take, leaving those builds incremental.

**The rule: build wall time here has ±4× variance on identical code and cannot carry a
performance claim.** A single fast arm is not a baseline. Compare distributions across
≥5 runs against the *unmodified* tree, and treat an arm with implausibly low variance
as suspect before treating it as good news.

## Background

- [`FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](FPCACHE_UNDERSIZED_AT_LOW_RAM.md) — the sizing
  half of this cache's behaviour under pressure; `evict_mapped` is the counter that
  tells the two apart
- [`SELFHOST_ZERO_PAGE_HUNT.md`](SELFHOST_ZERO_PAGE_HUNT.md) — the investigation this
  came out of, and the arms in §4
- `src/file_page_cache.rs` module docs — the refcount invariant that makes evicting a
  mapped entry free nothing
- [`../reference/subsystems/memory.md`](../reference/subsystems/memory.md) § "Shared
  file pages"
