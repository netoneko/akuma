# cargo null-`Rc` — memory reference & flag audit (in progress, 2026-08-08)

Working notes for the defect in [`proposals/CARGO_HEAP_NULL_RC.md`](../../proposals/CARGO_HEAP_NULL_RC.md):
during an in-guest `-j4` self-host build, `cargo` dereferences a null `Rc` and the
build dies with `EXIT=139`. Safe Rust cannot construct a null `Rc`, so a live
pointer qword in cargo's anonymous heap read back as zero — a kernel
memory-management bug.

This file records what the instrumentation has established, what it has ruled
out, and the theories still standing. It is a **live investigation log**, not a
conclusion: statements are tagged with the evidence behind them. Branch
`stabilize-devbox`.

---

## 1. The one hard correlation, from the original autopsy

In the crashing run (run 3 of 2026-08-07), the `[EAGER-UPGRADE]` repair fired on
cargo's heap page `0x314da000` **six log lines and one 10 ms tick** before the
process faulted dereferencing a pointer loaded from that same page:

```
85336: [EAGER-UPGRADE] pid=17 as_owner=17 va=0x314da000 flags=0x60000000000040
85343: [WILD-DA] pid=17 FAR=0x0 ELR=0x104e48c8 last_sc=222
         x0=0x314da660   ← ldr x8,[x0,#288] → 0x314da780, page 0x314da000
```

Those were the only two `[EAGER-UPGRADE]` lines in 86k lines of log, and both were
pid 17 (cargo) on heap pages.

`[EAGER-UPGRADE]` (`src/exceptions.rs`, the last arm of the permission-fault
block) fires on exactly one state: **a write fault on a VALID but read-only PTE,
inside an eager `mmap` region whose recorded flags say RW, where
`cow_ref_get(pa) == 0` and no lazy region covers the page.** It forces the PTE to
RW and resumes — which is why the process survived another 20 ms before reading a
corrupt value.

---

## 2. What the instrumentation established

Instruments added on this branch (all boot-suite tested, see
`src/process_tests.rs`):

| Instrument | Question it answers |
| --- | --- |
| `pmm::is_page_free(pa)` | Is the PA behind a live PTE *also* on the free list? |
| free ledger (`record_free` / `last_free_record`) | Which thread released this frame, how recently? |
| poison quarantine (`config::PMM_UAF_QUARANTINE`) | Did anyone write through a freed frame? |
| CoW event ring + durable bitset (`config::COW_REF_LEDGER`) | Was this frame ever CoW-shared, and by whom? |
| `MADV_DONTNEED` audit counters | Is that handler's divergence from Linux being exercised? |
| `[MPROT-WIDEN]` | Does an `mprotect` upgrade record "writable" outside its own range? |

### 2.1 The anomaly is common, and not itself fatal — OBSERVED

`[EAGER-UPGRADE]` fired in **both** instrumented runs. Run 1 fired one and still
went **green** (`EXIT=0`, 109 crates, 9m52s). So the anomaly is a precondition,
not the trigger; something else has to coincide with it to corrupt data.

### 2.2 Not a premature free — STRONG NEGATIVE EVIDENCE

The leading theory going in was a frame handed back to the PMM while a process
still mapped it, with the next `alloc_page_zeroed` wiping it under its live owner.
The forensic dump at the anomaly says otherwise:

```
[EAGER-UPGRADE] pid=9 va=0x31b6f000 pa=0x9ec99000 FREE=false cow_ref=0
                tracked=true last_free=(tid=-1 age=-1) head=0x38,0xff000000000b,...
```

- `FREE=false` — the frame is not on the free list.
- `tracked=true` — the address space legitimately owns it in `user_frames`.
- `last_free` absent — never freed inside the ledger window.
- head words are live data, **not zeros and not poison**.

Independently: **`PMM-UAF=0` and `PMM-QUAR-DF=0` across a complete 10-minute 4-way
self-host build**, with the detector proven to fire every boot (the self-test's
deliberate use-after-free is caught). That is meaningful negative evidence against
the whole premature-free family.

### 2.3 CoW involvement — UNTESTED, previously overclaimed

An earlier reading of `[COW-HIST] no recorded reference events` was written up as
"this frame was never CoW-shared". **That inference was too strong.**
`cow_share_and_demote_range` emits one `cow_ref_inc` per shared page, so a single
fork of a large process can evict the entire 4096-entry ring; "no events in the
window" may mean *aged out*.

Fixed by adding a durable one-bit-per-frame record (`COW_EVER`, sized from RAM at
`pmm::init`, ~1 KiB per 32 MiB) that never ages out. The report now distinguishes
the three cases explicitly:

```
[COW-HIST] pa=… no events in window: NEVER shared (durable bitset clear)
[COW-HIST] pa=… no events in window: shared earlier, detail aged out
[COW-HIST] pa=… no events in window: instrument off — says nothing
```

Until a run produces the first of those, **CoW is neither implicated nor
exonerated.**

---

## 3. The leading theory: `mprotect` widens an upgrade across a whole region

`update_eager_region_flags` (`crates/akuma-exec/src/process/children.rs`) sets
`reg.flags = new_flags` on **every region the range overlaps**, without splitting:

```rust
for reg in r.iter_mut() {
    if reg.start_va < range_end && reg.start_va + reg.len_bytes() > range_start {
        reg.flags = new_flags;          // whole region, however small the range
    }
}
```

Its doc comment argues this is safe, and the argument is sound **for a
downgrade**:

> "the fault handler only ever uses these flags to grant a write, so widening the
> recorded range of a *downgrade* can never turn a legitimate SIGSEGV into a
> silent success"

The code applies the same widening to **upgrades**. An
`mprotect(sub_range, PROT_READ|PROT_WRITE)` records "writable" for every page in
the region, including pages userspace deliberately left read-only or `PROT_NONE`.
That record is exactly the input the `[EAGER-UPGRADE]` arm reads before granting a
write — so the handler promotes a protected page and lets the write through,
which is the one outcome the comment claims is impossible.

**Reaching the firing state needs two `mprotect` calls on one region:** one that
downgrades a sub-range (clobbering the record to RO), and a later one that
upgrades some sub-range (flipping the whole record back to RW) while the first
sub-range's PTEs are still RO.

This fits every forensic field observed: no CoW reference needed, no free needed,
frame owned and tracked, content intact, region claiming RW while the PTE says
otherwise.

**Consequence if it fires on a guard page or a deliberately-RO page:** a stray
write that should have raised SIGSEGV instead lands in live neighbouring data.
That is a corruption primitive sufficient to explain a zeroed/garbage pointer
field in cargo's heap, and it requires an actual overrun to occur — which matches
the load dependence and the ~1-in-5 rate.

### 3.1 The stated reason not to split is contradicted by existing code

The comment justifies not splitting with:

> "`MmapRegion` keys its frame list to `start_va`, and splitting one would have to
> split `frames` in step"

`sys_munmap` (`src/syscall/mem.rs`) **already does exactly that** — it removes the
region, splits `frames` with an iterator, re-pushes the suffix at
`start_va + unmap_pages * 4096`, and carries `flags` across:

```rust
let mut iter = reg.frames.into_iter();
let prefix: Vec<PhysFrame> = (0..unmap_pages).filter_map(|_| iter.next()).collect();
let remaining: Vec<PhysFrame> = iter.collect();
r.push(MmapRegion { start_va: reg.start_va + unmap_pages * 4096,
                    pages: region_pages - unmap_pages,
                    frames: remaining, flags: reg.flags });
```

So the fix has precedent in the same subsystem: split the eager region on a
partial `mprotect` the way `munmap` splits it on a partial unmap, and give each
piece its own flags. The lazy path already splits properly
(`LazyRegions::update_flags`, up to three pieces).

**Status: found by inspection, consistent with every observation, NOT yet observed
firing.** `[MPROT-WIDEN]` logs the region, the requested range, and how many pages
outside the call just became recorded-writable. `[MPROT-WIDEN]` and
`[EAGER-UPGRADE]` on the same region is the proof chain, and it should be in hand
before the behaviour is changed — fixing it first would remove the evidence.

---

## 4. Standing divergence points found during the audit

Each is a place where one bookkeeping structure can drift from another. Only D1 is
implicated so far; the rest are recorded so the audit is not repeated.

| ID | Divergence | Status |
| --- | --- | --- |
| **D1** | `update_eager_region_flags` widens an *upgrade* across a whole region (§3) | **Leading theory** |
| D2 | `file_page_cache::lookup_and_ref` takes its `cow_ref_inc` **outside** the `PAGES` lock; a concurrent `invalidate_inode`/`shrink` can free the frame in that window, after which the inc resurrects a count on a freed frame and the caller maps it | Suspected, not observed. `inval=1466` in the crashing run made the window reachable; `evict=0` means `shrink` was idle |
| D3 | `file_page_cache::insert`'s lost-race path returns early without inserting but still runs `cow_ref_inc` on its own private frame → permanent +1 | Code-visible leak |
| D4 | The three CoW-break sites call `cow_ref_dec` directly and discard the "last reference" return; if every sharer breaks, the original frame is freed by nobody | Code-visible leak |
| D5 | `MADV_DONTNEED` memsets the *physical frame* (Linux drops the *mapping*), with no check for `cow_ref > 0` or cache membership; and rounds an unaligned start **down** where Linux returns `EINVAL` | Instrumented (`[MADV]` counters); no data yet |
| D6 | Eager regions register **no** lazy region, so `MmapRegion.flags` is the fault handler's only repair input — which is what makes D1 dangerous rather than merely untidy | Structural |
| D7 | `try_evict_ro_page` evicts *any* RO page inside a `LazySource::File` region, which would include a CoW-demoted anon page if a VA range were stale or recycled | Suspected |
| D8 | `sys_munmap`'s eager path returns before `munmap_lazy_regions_in_range`, so a lazy region covering the same VA would survive the unmap | Suspected |

---

## 5. Ruled out for this defect

- **Premature free / use-after-free** (§2.2) — `PMM-UAF=0` over a full build with a
  proven-live detector, plus `FREE=false`/`tracked=true`/no-free-record at the
  anomaly.
- **`ENOSYS`/errno-as-pointer** — the class the `SYSCALL_ERRNO_DIAG` flag exists
  for. The crashing run's syscall ring contains **zero** `ENOSYS`, `EFAULT` or
  `EINVAL` results; the only negative result anywhere is `-110` (`ETIMEDOUT`) from
  `futex`. The fault also has `FAR=0x0` with the null loaded *from memory*
  (`ldr x8,[x0,#288]`), not from `x0` after an `svc`. Enabling that diagnostic
  would add tens of thousands of `readlinkat`-`EINVAL` lines per build and hide the
  two lines that matter.

---

## 6. Noise, and things not to chase

- `[BKL] stuck tag=511` storms — known separate class, hundreds per build.
- Two boot-suite tests fail on this branch **and on a pristine tree**
  (`thread_slot_reclaim_on_spawn` `hot_reclaim=206/208`, `retired_reclaim_ab`
  recovering 745p against a 768p threshold). Verified by stashing all changes and
  re-running: identical failures, identical 745p. Pre-existing, unrelated.
- The single-core `release` boot suite stalls in the same place with and without
  these changes (~3130 lines, after `drivers-bkl-drop`). Also pre-existing.

---

## 7. Instrument hazards learned the hard way

- **`free_page` must not call `read_current_pid()`.** It resolves through
  `THREAD_PID_MAP` and the process table, and `free_page` is reachable from inside
  both (a `Process` drop frees every frame of its address space) — a non-reentrant
  `Spinlock` deadlock that wedged the first instrumented boot at 554 lines. Record
  `current_thread_id()` instead; it is a register read.
- **The quarantine must surrender its hold-back before an allocation fails.**
  `quarantine_drain_all` sits on `alloc_page`'s pressure ladder so 512 parked
  frames can never be the reason a build OOMs.
- **A "no record" sentinel must not be printed as a plausible number.** The first
  version printed `last_free=(tid=4294967295 age=38240)` where the age was computed
  against a default seq of 0 — a large number that reads exactly like a real,
  innocent age. It now prints `-1` for both, with the meaning in the line.

---

## 8. Next steps

1. Run the `-j4` build with `[MPROT-WIDEN]` armed until it fires (or several runs
   establish that it does not). Correlate against `[EAGER-UPGRADE]` by region.
2. If correlated: split the eager region on partial `mprotect` (§3.1), keeping the
   existing behaviour behind a same-binary A/B toggle per
   `docs/reference/subsystems/locking.md` rule 5, and re-run.
3. Either way, close D3/D4 (both are code-visible leaks) and decide on D5 —
   `MADV_DONTNEED` should drop the mapping rather than memset a possibly-shared
   frame, and should reject an unaligned start with `EINVAL` like Linux.

## Background

- [`proposals/CARGO_HEAP_NULL_RC.md`](../../proposals/CARGO_HEAP_NULL_RC.md) — the
  original problem statement and reproduction recipe.
- [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md) —
  "Status (2026-08-07)", Defect A and B.
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) §10 — the `MADV_WILLNEED`
  zero-fill corruption, the closest prior defect in this family.
