# The file-page dedup cache is sized wrong for small boxes

**Status: OPEN.** Found 2026-08-10 on branch `trim-fat-sshd` while measuring
free RAM at the 4 MB floor. Nothing here is fixed; this doc records the evidence
and separates what was measured from what was inferred.

## Symptom

On a 4.5 MB extreme box, a handful of sequential SSH sessions — each running an
ordinary pipeline like `echo … | head -1` — ends in:

```
/bin/sh: can't fork: Out of memory
```

There is **no `[OOM]` line** in the serial log, so this does not look like the
kernel-heap wall from `EXECVE_STACK_LEAK_OOM_HANG.md`. The box stays up and the
console keeps printing; only `fork` fails.

## Measured

Free physical memory across one 4.5 MB boot (1152 pages total), from the
`[FSCACHE]`/`[FPCACHE]` heartbeat lines:

| moment | `pmm_free` |
|---|---|
| post-boot idle | 678 pg (2712 KB) |
| after ~9 sequential SSH command sessions | 102 pg (408 KB) |

The cache line at that second sample:

```
[fpcache] shared file-page cache enabled, cap=144 pages
[FPCACHE] entries=144 hits=77048 misses=13175 evict=13031 inval=0
```

`entries == cap`, and `evict` is 98.9% of `misses`. A cache that evicts on
almost every miss is, by definition, below its working set.

The cap comes from `src/file_page_cache.rs:104`:

```rust
let cap = (total_ram_bytes / 8) / 4096;
```

called once from `src/fs.rs:133` at mount. That is `RAM_bytes / 32768`:

| RAM | cap (pages) | cap (KB) |
|---|---|---|
| 4.0 MB | 128 | 512 |
| 4.5 MB | 144 | 576 |
| 8.0 MB | 256 | 1024 |
| 64 MB | 2048 | 8192 |

And the binary that every process on that box *is*:

```
/bin/busybox   1,116,408 bytes on disk
               .text 900,876 + .rodata 183,151 = 1,084,027 = 265 RO pages
```

Only the RO/RX mapped segments are cacheable (`is_shareable_mapping`,
`src/file_page_cache.rs:112`, requires `AP_RO_ALL`), so **265 pages** is the
figure that matters, not the 273 the file size implies. `/bin/sh`, `head`, `wc`,
`grep`, `sed` are all symlinks to this one binary, so a single pipeline is 2–3
concurrent busybox images.

**Break-even: the cache cannot hold one busybox until RAM ≥ 265 × 32768 ≈
8.3 MB.** Every box at the documented 4–4.5 MB floor is below that line by
roughly 2×.

At 64 MB (cap 2048) the identical workload — same disk, same kernel, same
pipeline set — completed first try with no OOM.

## Inferred (mechanism — read from the code, not instrumented)

Eviction does *not* free a mapped frame. From `insert()`
(`src/file_page_cache.rs:161`, eviction at ~203–207):

```rust
EVICTIONS.fetch_add(1, Ordering::Relaxed);
// Drop the cache's reference. Frees only if nobody still has it mapped;
// otherwise the last unmapper frees it through the same path.
crate::pmm::free_page(PhysFrame::new(pa));
```

What eviction destroys is the **dedup entry**, not the page. So the failure mode
is not "the cache lost some memory" but "two processes stopped sharing":

1. Process A faults page *N* of busybox → miss → read from ext2 → frame F1 →
   `insert()`.
2. The insert pushes the map over `cap`, so some other entry — possibly page
   *N−k* that process A is still executing from — is evicted. F1 survives
   because A has it mapped; its dedup entry is gone.
3. Process B faults that same evicted page → **miss** (no entry) → reads it from
   ext2 again into a **new** frame F2.
4. A and B now hold two physical copies of one identical read-only page.

With cap ≈ half the binary, this happens continuously, and N concurrent busybox
instances trend toward N private copies of their text instead of one shared set.
That is consistent with `pmm_free` falling 678 → 102 while only short-lived
shells ran.

**This chain is not directly proven.** I did not instrument duplicate-frame
counts, and did not confirm the freed pages return after the sessions exit — a
single 30 s sample was taken. A per-inode duplicate-frame counter, or watching
`pmm_free` recover after the volley, would settle it and separate this from an
ordinary per-session leak. Treat the causal link between the thrash and the
`fork` OOM as the leading hypothesis, not an established fact.

## Why `RAM/8` is the wrong shape

The module's own docstring (`src/file_page_cache.rs:96–99`) makes the argument
against its own formula:

> This cache is a *deduplicator*, not an extra consumer: an entry whose frame is
> still mapped costs nothing beyond the map node, since that frame was going to
> exist anyway. Only entries with zero mappers hold memory that would otherwise
> be free, which is why the cap can be generous relative to the ext2 block cache.

If a mapped entry is nearly free, the cap should not be a fraction of RAM. The
quantity it needs to cover is a property of the **binaries being mapped**, and
that does not shrink when the box does. `RAM/8` makes the table smallest exactly
where deduplication is most valuable: on a big box you can afford the duplicate
copies, on a 4 MB box you cannot.

Undersizing this cache therefore *costs* memory rather than saving it — the
opposite of what a cache cap is normally for.

## Fix directions (none implemented)

1. **Floor the cap at the largest resident binary.** Something like
   `max(RAM/32768, 320)` covers busybox with headroom. Cheap, but the floor is a
   magic number that goes stale when the binary set changes.
2. **Only count zero-mapper entries against the cap.** Directly encodes the
   docstring's argument: mapped entries are free, so don't evict them to satisfy
   a budget they don't consume. Needs a mapper count per entry (the refcount
   already exists via `cow_ref_inc`/`free_page`), and a cap that bounds only the
   unmapped tail.
3. **Evict on memory pressure instead of on count.** Keeps entries while frames
   are cheap and sheds the zero-mapper tail when the PMM is actually tight.
   Best behaviour, most work, and it needs a pressure signal the PMM does not
   currently expose to this module.

(2) looks like the right one: it is what the docstring already claims the design
does, and it makes the cap safe to raise without pinning memory.

### 4. Or attack it from the other end: ship a smaller busybox

The cache is undersized *relative to the binary*, and the binary is the other
half of that ratio. A 2026-08-10 investigation rebuilt busybox 1.38.0 (static
musl aarch64) with only the applets an interactive build box needs, plus a
full-featured `ash`:

| build | file bytes | RO pages |
|---|---:|---:|
| `bootstrap/bin/busybox` as shipped | 1,116,408 | **265** |
| minimal rebuild, same applet set + full ash | 332,616 | **79** |
| toybox, equivalent applet set + `toysh` | 350,552 | 79 |

**79 pages fits under the 144-page cap** — so a minimal busybox makes the
dedup cache work at 4.5 MB without touching this module at all. It still
consumes 55% of the cache, so it dedups only while little else competes; the two
fixes are complementary, not alternatives.

Toybox is *not* the way to get there despite the similar size: `toysh` is still
in `toys/pending/`, defaults to `n`, and fails `set --`, `read`, `command`,
`set -e` and `trap EXIT` — it cannot run our shell scripts. The size win is
available from busybox with no change in shell semantics. Full measurements,
build recipe and the toysh evidence:
[`BUSYBOX_TOYBOX_SIZING.md`](BUSYBOX_TOYBOX_SIZING.md).

Whatever lands, the guard against regression is the ratio, not the absolute
numbers: **`evict` should not track `misses`.** If they move together, the cache
is below its working set again.

## Reproducing

```bash
scripts/build_extreme_size.sh
MEMORY=4608K SNAPSHOT=1 INSTANCE=0 scripts/cargo_runner.sh \
    target/aarch64-unknown-none/extreme-size/akuma 2>&1 | tee fpcache.log
```

Then run several separate SSH sessions, each with a small pipeline
(`echo x | head -1`, `echo y | wc -c`, …) rather than one session doing
everything — the point is repeated concurrent busybox images. Watch:

```bash
grep -a FPCACHE fpcache.log     # evict climbing with misses, entries pinned at cap
grep -a FSCACHE fpcache.log     # pmm_free falling and not recovering
```

`grep -a` is required: QEMU emits a control byte that makes plain `grep` treat
the log as binary.

Contrast run — same everything at `MEMORY=64M` (cap 2048) — completes clean.

## Related

- Cache sizing has bitten this file before in the other direction:
  `docs/archive/FILE_PAGE_CACHE_MMAP_AMPLIFICATION.md`.
- `docs/reference/subsystems/memory.md` — PMM / page lifecycle.
- `docs/archive/BUILTIN_SSH_REMOVAL.md` — the measurement session that surfaced
  this; its free-RAM tables are the same boots quoted above.
- `acceptance/05_meow_tcc_extreme_4mb.md` — the 4.5 MB profile this degrades.
  Its recorded ~2520 KB post-boot idle predates the observation, and the compile
  peak it documents (~1988 KB free) leaves little room for duplicate text.
- `docs/archive/EXECVE_STACK_LEAK_OOM_HANG.md` — the *other* small-box
  out-of-memory class, distinguished by `[OOM]` lines in the log. This one has
  none.
