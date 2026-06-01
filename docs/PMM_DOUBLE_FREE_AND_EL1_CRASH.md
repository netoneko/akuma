# Investigation: meow.log EL1 crash → PMM double-free hardening

## Date

2026-06-01 (branch `expand-acceptance`, **not yet committed**)

## Status

- ✅ Crash analyzed, mechanism identified, deterministic reproducer written.
- ✅ PMM-layer fix landed (accounting correctness + double-free detection).
- ✅ Boot healthy, both self-tests pass, no real-path double-frees at runtime.
- 🔲 **Open for tomorrow:** the concurrency variant (deferred TLB flush + vfork
  shared-AS) is *detected* but not *prevented*; decide whether to commit and
  whether to harden the refcount further. See "Remaining work".

---

## The crash

One real crash, at the tail of `meow.log` (a 128 MB / low-memory run). Everything
else labeled "EL1" in `meow.log`/`meow1.log` is deliberate boot self-test fault
injection (`EC=0x25` redirection tests, all passing); the `panic=1` in the SSH
status line is a static stat counter, not a live panic.

```
[Exception] Sync from EL1: EC=0x22, ISS=0x0
  ELR=0x91150042f00011c2, FAR=0x91150042f00011c2, SPSR=0x90102045
  Thread=0, TTBR0=0x404d5000, ...
  SP=0x409ff580, SP_EL0=0x17fffeae9122b280
  Thread 0 kernel stack: base=0x40700000, top=0x40800000
  WARNING: Kernel SP outside thread's stack bounds!
  No current user process
```

This is **kernel-state corruption**, not a normal fault:

- `EC=0x22` = PC alignment fault — the CPU branched to a non-instruction address.
- `ELR == FAR == 0x91150042f00011c2` — a 64-bit garbage value (high bits set),
  not any valid kernel/user address. The saved PC was overwritten with data.
- `SP=0x409ff580` is **above** Thread0's stack top (`0x40800000`), ~2 MB *into the
  kernel heap*; `SP_EL0` is also garbage.

SP pointing *up* into the heap (not down past the stack base) rules out a simple
stack overflow. The signature — garbage ELR + heap-valued SP + garbage SP_EL0 on
the idle scheduler thread — is **a freed kernel page being reused and clobbering
Thread0's saved context**.

The 256 MB run (`meow1.log`) ran ~31 min and was truncated, not crashed — so the
fault is load/timing dependent and (at minimum) correlated with the low-memory
regime the recent commits target.

---

## Correlation with recent commits

`meow.log` post-dates all of:

| Commit | Subsystem | Relevance |
|---|---|---|
| `944ab43` / `021050b` low-memory heap layout | `compute_heap_size`, main.rs | At 128 MB, `code_and_stack = 8 MB` → `heap_start = 0x40800000` = **Thread0 boot-stack top**. Boot stack now butts directly against the heap, no guard page. Consistent with the symptom; not the cause. |
| `8e2f625` / `ba60d72` faster munmap | `mmu/mod.rs`, `syscall/mem.rs` | `user_frames` `Vec<PhysFrame>` → `BTreeMap<PA, u32 refcount>`; deferred per-region TLB flush. **Prime suspect.** |
| `8cf6144` vfork fast-path | `process/mod.rs` | Shared-L0 child (`new_shared`) dropped via refcount on exec. Secondary suspect (concurrency). |

---

## Root cause (the part that is fixed)

`pmm::free_page` (`src/pmm.rs`) decremented `ALLOCATED_PAGES` **unconditionally**,
even when the bitmap allocator's `!is_free` guard turned the actual free into a
no-op:

```rust
// BEFORE
crate::irq::with_irqs_disabled(|| {
    let mut pmm = PMM.lock();
    pmm.free_page(frame);              // bitmap free is guarded by !is_free
    ALLOCATED_PAGES.fetch_sub(1, ...); // but this ran ALWAYS
})
```

Consequences of any double-free of a non-CoW page (`cow_ref_dec` returns `true`
for an untracked PA, so the free proceeds):

1. **Accounting corruption** — `ALLOCATED_PAGES` drifts on every double-free even
   when the bitmap guard prevented the re-mark. `pmm::stats()` free count
   over-reports.
2. **Heap corruption (the crash)** — if the page was *reallocated* between the
   two frees, the bitmap guard does **not** protect (`is_free` is now false for
   the new owner), so the stale free marks an in-use page free → it is handed to
   a second owner → garbage written through the alias = the Thread0 corrupted
   saved context.

### Where a double-free can come from

`user_frames` is a multiset (`PA → count`) of within-AS references; `Drop`
frees each PA `count` times (`mmu/mod.rs:809`). This is **correct as long as
`cow_ref` was incremented `count` times to match.** Verified that the real paths
keep them in lockstep:

- CoW share (`process/mod.rs:1361`): `cow_ref_inc(pa)` + `child_as.track_user_frame(pa)`
  per mapped page — 1:1.
- CoW fault (`exceptions.rs:819`): `track_user_frame(new)` + `remove_user_frame(old)`
  + `cow_ref_dec(old)` — paired.

So a double-free requires a *caller* whose `track_user_frame` / `cow_ref`
obligations desync. There are ~30 `track_user_frame` call sites, each hand-
maintaining the pairing — fragile. The reproducer constructs that desync
explicitly (track a singly-allocated PA twice with no matching `cow_ref`).

---

## The fix (PMM-layer defense + observability)

`src/pmm.rs`:

- `BitmapAllocator::free_page` now returns `FreeOutcome { Freed, DoubleFree, OutOfRange }`
  instead of silently swallowing an already-free page.
- `pmm::free_page` decrements `ALLOCATED_PAGES` **only on a real allocated→free
  transition**; a refused double-free increments `DOUBLE_FREE_COUNT`.
- `pmm::double_free_count()` exposes the counter; `pmm::discount_double_frees(n)`
  lets a self-test discount its own deliberate double-free.

`src/main.rs`:

- The periodic `[Mem]` line appends `| DOUBLE-FREE=N` **only when non-zero**, so a
  `track_user_frame`/`cow_ref` desync surfaces as a visible, contained signal
  under load instead of silent heap corruption + a later mystery EL1 fault.

This is **defense, not a refcount rewrite**: it cannot affect the (correct)
lockstep paths, fixes the accounting bug unconditionally, and turns the realistic
back-to-back double-free (the `Drop` case, frees in a tight IRQ-disabled loop)
into a safe, counted no-op.

---

## Tests (`src/process_tests.rs`, boot self-test suite)

Both run under IRQs-disabled so PMM accounting is deterministic (no concurrent
allocation noise):

- `test_munmap_teardown_conserves_pmm` — positive control. Maps 64 distinct
  pages, tears them down via the real `unmap_and_free_page` → `free_page` path,
  drops the AS, asserts the PMM free count returns to baseline. **PASSES.**
- `test_aliased_pa_not_double_freed` — reproducer. Allocates **one** page, maps
  it at two VAs and tracks it for each (the `count>1` state the design admits),
  drops the AS so `Drop` frees it twice. Asserts (a) the free list / counter are
  conserved and (b) the redundant free was *detected and refused*
  (`df_delta == 1`); then discounts its own double-free so `[Mem]` stays clean.
  **FAILED before the fix** (free count drifted +1); **PASSES after.**

Verified on a 256 MB boot: both PASS, boot reaches SSH, no EL1/panic, and
`DOUBLE-FREE` never appears in any runtime `[Mem]` line (real paths balanced).

---

## Remaining work (tomorrow)

1. **Decide on commit.** The PMM hardening is self-contained and safe; commit it
   (with the two tests) on `expand-acceptance`.
2. **The concurrency variant is detected, not prevented.** Deferred TLB flush
   (`ba60d72`: `unmap_*_no_flush` clears the PTE, frees the page, and flushes the
   TLB only once per region *after* the loop) combined with the vfork shared-AS
   fast-path (`8cf6144`) means a sibling/child thread sharing the L0 can still
   hold a stale TLB entry for a page that was already returned to the PMM and
   reallocated. That realloc-interleaving double-free is *not* caught by the
   `is_free` guard (the page looks legitimately allocated to its new owner). The
   `DOUBLE-FREE` counter is now the instrument to catch a desync; run the
   suspected workload at 128 MB and watch for it.
3. **Optional deeper hardening** (only if (2) reproduces): either flush the TLB
   *before* returning each page to the PMM on the shared-AS path, or make the
   refcount self-enforcing so `track_user_frame`/`cow_ref` cannot desync.
4. **Repro under lldb+gdbstub** at 128 MB (`MEMORY=128`, `INSTANCE=1 GDB=1`,
   `:1235`) with a hardware watchpoint on Thread0's saved-context region to catch
   the stray write in the act and confirm which path frees the live page.

## Files touched

- `src/pmm.rs` — `FreeOutcome`, `DOUBLE_FREE_COUNT`, fixed decrement, accessors.
- `src/main.rs` — `[Mem]` `DOUBLE-FREE=N` marker.
- `src/process_tests.rs` — `test_munmap_teardown_conserves_pmm`,
  `test_aliased_pa_not_double_freed`.

See also: `docs/COW_OPTIMIZATIONS.md` (the munmap/vfork work), `docs/AI_DEBUGGING.md`
(lldb+gdbstub setup).
