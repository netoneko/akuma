# Investigation: meow.log EL1 crash → PMM double-free hardening

## Date

2026-06-01 (branch `expand-acceptance`, **not yet committed**)

## Status

- ✅ Crash analyzed, mechanism identified, deterministic reproducers written.
- ✅ PMM-layer hardening (accounting correctness + double-free detection).
- ✅ **Session 2:** refcount-aware unmap (over-free *prevented*, not just guarded);
  3 teardown self-tests pass at 64 MB.
- ✅ **Session 2:** found and fixed the *actual* `EC=0x22` cause — the ELF
  **heap-slurp** in `spawn.rs` (unrelated to the double-free). apk now loads & runs.
- 🔲 **Open:** a *third* crash remains — `EC=0x21` garbage-PC on Thread0 under
  apk's heavy mmap/munmap churn (~55 regions / 27 MB, not OOM). See "Remaining work".

> **Important correction (session 2).** This doc originally attributed the
> `meow.log` `EC=0x22` crash to the `user_frames` double-free. That was wrong.
> Fixing the double-free did **not** stop `apk search`@64 MB crashing (identical
> `EC=0x22`, byte-identical garbage registers). The real cause was the ELF
> heap-slurp (Bug 2 below). The double-free was a genuine, separate bug — now
> properly fixed — but it was **not** this crash. Lesson: any corrupted-PC kernel
> fault produces a similar `EC=0x22` "garbage ELR/SP" signature, so the signature
> alone does not identify the culprit.

## Summary: three distinct bugs

| # | Bug | Status | Effect |
|---|---|---|---|
| 1 | `user_frames` refcount over-free (unmap/Drop ignore the refcount) | ✅ fixed | aliased PA freed N times; latent, PMM-guard-hidden |
| 2 | ELF **heap-slurp**: `spawn.rs` reads whole binary into the 8 MB heap | ✅ fixed | **the actual `EC=0x22`**; 5 MB apk exhausts heap @ MEMORY=64 |
| 3 | apk crashes under heavy mmap churn after Bugs 1+2 fixed | 🔲 open | `EC=0x21` garbage-PC Thread0, ~27 MB used, not OOM |

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

## The fix, part 2: refcount-aware unmap (session 2) — Bug 1 prevented at source

The PMM-layer defense above *contains* the over-free; session 2 *prevents* it.
The "faster munmap" refactor added a refcount to `remove_user_frame` but never
wired it into the **free decision**:

- `unmap_and_free_page` called `remove_user_frame` (which decremented the count)
  and then returned the frame **unconditionally** — so the caller freed it even
  when the count was still > 0 (page still mapped at another VA).
- The eager munmap loops (`syscall/mem.rs`) did `remove_user_frame(frame);
  free_page(frame);` unconditionally.
- `UserAddressSpace::drop` freed each PA **`count` times** (`for _ in 0..count`).

A PA mapped at N VAs is **allocated once** but freed N times — the over-free.

**Fix (`crates/akuma-exec/src/mmu/mod.rs`, `src/syscall/mem.rs`):**

- `remove_user_frame` now returns `bool` (and is `#[must_use]`): `true` iff it
  dropped the **last** reference (count hit 0) — i.e. the caller now owns the
  free. `false` means "still mapped elsewhere, or not tracked in this AS"
  (e.g. a `new_shared` vfork view, whose frames the L0 owner frees).
- `unmap_and_free_page` returns `Some(frame)` **only when `remove_user_frame`
  returned true**; otherwise `None` (PTE cleared, frame not freed).
- The eager munmap loops free `only if remove_user_frame(frame)`.
- `UserAddressSpace::drop` frees each distinct PA **once** (the count is a
  *mapping* refcount, not an alloc count).
- CoW call sites (`exceptions.rs`) keep their existing behavior via `let _ =`
  (their free is governed by `cow_ref_dec`, not `user_frames`).

Untracked-frame policy: **don't free** (return `None`/`false`). A leak is
recoverable and visible in the counters; an over-free hands a live page to a new
owner and crashes the kernel. Shared (vfork) views legitimately have an empty
`user_frames`, so "untracked here" is the normal, correct case for them.

---

## Bug 2: the ELF heap-slurp — the *actual* `EC=0x22` cause

After Bug 1 was fixed, `apk search`@64 MB still crashed with the **identical**
`EC=0x22` and byte-identical garbage registers. Bisecting by binary size and
boot-fresh runs isolated it:

- `/bin/hello` (72 KB), `/bin/echo2` — run clean at 64 MB.
- `apk` (**5 MB**) — crashes, **even as the very first command**, with PMM
  **unchanged from boot** (no physical pages allocated yet) but the **kernel heap
  at 7.2 MB / 8 MB (86 %)**.
- apk runs fine at 256 MB (64 MB heap).

Root cause: `spawn_process_with_channel_ext` (`crates/akuma-exec/src/process/spawn.rs`)
loaded the ELF via `read_file(elf_path)`, which returns the **entire binary as a
`Vec<u8>` on the kernel heap**. For a 5 MB executable that alone consumes most of
the 8 MB `MEMORY=64` heap (the heap scales with RAM, hence 64 MB-only). The
deferred, demand-paged loader (`from_elf_path`) already existed but was only a
*fallback* used when `read_file` **failed**.

This explains every observation the double-free theory could not: apk-specific
(size), 64 MB-only (heap size), reproduces as first command, **PMM untouched**
(heap — not PMM — exhausted), deterministic.

**Fix (`spawn.rs`):** size-gate the loader. Files larger than `HEAP_SLURP_MAX`
(1 MiB) use the demand-paged `from_elf_path` path (segments mapped lazily from
the file, flat heap use regardless of binary size); smaller binaries keep the
well-trodden whole-file path. Both the interactive (`spawn_process_with_channel_cwd`)
and non-interactive (`exec_streaming_cwd`) SSH exec paths funnel through this one
function, so the single change covers both.

**Result:** apk now loads with the heap at ~2 MB (not 7.2 MB) and executes — it
gets through 55 mmap regions / ~27 MB before hitting Bug 3.

---

## Bug 3 (open): crash under heavy mmap churn

With Bugs 1 + 2 fixed, `apk search llama`@64 MB now runs substantially, then:

```
[Exception] Sync from EL1: EC=0x21, ISS=0x4
  ELR=0xb69c09c86dc7f288, FAR=0xb69c09c86dc7f288, SPSR=0x50100345
  Thread=0, TTBR0=0x404d4000, SP=0x409ff580, SP_EL0=0xcbf20bfb29ba4724
  Heap: 2.0/8 MB used   PMM: 3424/16384 free (~13 MB)   (NOT OOM)
```

- `EC=0x21 ISS=0x4` = instruction abort, translation fault L0 — the kernel
  fetched an instruction from an **unmapped** garbage PC (vs Bug 1/2's `EC=0x22`
  misaligned PC). Same class: a corrupted return/function pointer on Thread0.
- Happens after ~55 mmap regions and ~27 MB allocated (13 MB still free — not OOM).
- apk is mmap-heavy (282 mmap / 47 munmap at the crash) and **re-munmaps the same
  VA repeatedly** (e.g. `0x207b3000 full (4 pages)` ×4).

**Reproduced controllably with `forktest` (the userspace stressor).** Installed via
`pkg install forktest_parent forktest_child` (host serves `bootstrap/` on `:8000`;
the guest fetches `10.0.2.2:8000` directly — no Docker, no hostfwd):

- `forktest_child -mmap_test -mmap_alloc_mb 8` (few large allocs) — runs **clean**.
- `forktest_parent -num_children 2 -combined_stress -mmap_test -mmap_alloc_mb 4
  -goroutine_stress -duration 20s` — **crashes** at MEMORY=64, `EC=0x22`, Thread0,
  **at an `execve` of a forked child**.

**It is NOT a double-free, NOT a use-after-free, and NOT user-touches-kernel.**
The `DOUBLE-FREE` counter stayed **0**, `free_page` correctly gates on CoW, and
the faulting context is the **kernel's own boot stack** (`Thread=0`, `SPSR=EL1h`,
`SP_EL1=0x409ffb00`, "No current user process"). It is **kernel-corrupts-kernel**.

### Root cause (CONFIRMED via lldb+gdbstub): kernel heap overlaps the boot stack

gdbstub on the wedged VM showed `x29` **and** `x30` of the faulting frame both
garbage, and the kernel stack at `SP_EL1` filled with **uniform high-entropy data,
no call-frame structure** — i.e. `SP` pointed into a *data buffer*, and a function
epilogue `ldp x29,x30,[sp]; ret` loaded garbage → jumped to garbage.

The boot stack is fixed at `0x40900000–0x40A00000` (`BOOT_STACK_TOP =
KERNEL_BASE + 8 MB`, boot.rs). But `main.rs` reserved only `code_and_stack =
max(ram/16, 8 MB)`. The 8 MB constant **forgot the 2 MB `KERNEL_BASE` offset**:
the stack top is at `ram_base + 10 MB`, so at 64 MB `heap_start = ram_base + 8 MB
= 0x40800000` and the heap span (`0x40800000–0x41000000`) **contained the live
boot stack**. As kernel-heap usage climbed past ~1 MB under apk/forktest churn,
`Box`/`Vec`/path-string/header allocations landed at `0x409xxxxx` — on top of
thread 0's stack frames. The MMU protects kernel-from-user but **cannot protect
the kernel from its own allocator** when two kernel regions overlap.

The "Go binary" ASCII bytes (`"ec/src/r"`, `"pageallo"`) and apk code bytes were
**kernel-heap copies the kernel made while servicing the process** (lazy-region
path `String`s, ELF headers via `file_read_exact`, args/env, crypto buffers) —
kernel-owned data, not the user's pages.

Why 64-only: at ≥256 MB, `ram/16 ≥ 16 MB` so `heap_start = 0x41000000`, above the
stack — no overlap. The bug is the *low-memory sibling* of the high-memory VA
collision (`memory_over_2gb_va_collision`) and the `DEFERRED_THREAD_CLEANUP` stack
race (docs/STACK_CORRUPTION_ANALYSIS.md): the same "region boundary computed with
a wrong constant" class.

### Fix (applied, `src/main.rs`)

`code_and_stack` now also covers `BOOT_STACK_TOP + 1 MB guard`
(`stack_cover = (BOOT_STACK_TOP - ram_base) + 1 MB`), so `heap_start` is always
above the boot stack. A boot-time guard halts if `heap_start < BOOT_STACK_TOP`.
At 64 MB the layout is now Code+Stack **11 MB** (`…–0x40b00000`), Heap
`0x40b00000–0x41300000`, User 45 MB.

**Verified:** 5× `apk search` (llama ×4 + tcc) at MEMORY=64 — **zero kernel
crashes, zero double-frees, QEMU stays up**, uptime 49 s. apk's own userspace
SIGSEGV is now cleanly contained as `[exit code: -11]` (a separate, lower-severity
userspace matter — the kernel survives and reaps the process correctly).

---

## Tests (`src/process_tests.rs`, boot self-test suite)

Both run under IRQs-disabled so PMM accounting is deterministic (no concurrent
allocation noise):

- `test_munmap_teardown_conserves_pmm` — positive control. Maps 64 distinct
  pages, tears them down via the real `unmap_and_free_page` → `free_page` path,
  drops the AS, asserts the PMM free count returns to baseline. **PASSES.**
- `test_aliased_pa_not_double_freed` — reproducer. Allocates **one** page, maps
  it at two VAs and tracks it for each (the `count>1` state the design admits),
  drops the AS. After the session-2 refcount fix, `Drop` frees it **once**, so
  the test now asserts the free list / counter are conserved **and no double-free
  is even attempted (`df_delta == 0`)** — the PMM guard is a backstop, not a
  crutch. (Was asserting `df_delta == 1` against the PMM-guard-only fix.) **PASSES.**
- `test_unmap_and_free_respects_refcount` (session 2) — covers the munmap-path
  half. Maps one PA at two VAs (count == 2), then `unmap_and_free_page(va1)` must
  return `None` (still referenced) and `unmap_and_free_page(va2)` must return
  `Some` (last reference, freed once). Asserts PMM conserved, first→None,
  second→Some, `df_delta == 0`. **PASSES.**

Verified at **MEMORY=64**: all three PASS, boot reaches SSH, no spurious EL1/panic,
`DOUBLE-FREE` never appears in any runtime `[Mem]` line (real paths balanced).

---

## Remaining work

1. **Bug 3 (open) is the priority.** Deterministic `EC=0x21` garbage-PC on Thread0
   under apk's heavy mmap/munmap churn, not OOM. Approaches:
   - **lldb+gdbstub** (`MEMORY=64`, `INSTANCE=1 GDB=1`, `:1235`): break at the EL1
     instruction-abort vector, walk back to the corrupted return/fn-ptr and the
     kernel structure apk trampled.
   - Audit the **mmap-churn** paths: repeated same-VA munmap (observed
     `0x207b3000` ×4), lazy-region split/merge, deferred-TLB-flush windows.
   - Drive it under **`userspace/forktest` + `userspace/allocstress`** load so
     concurrent pressure is in play while apk churns.
2. **The concurrency variant of Bug 1 is detected, not fully prevented.** Deferred
   TLB flush (`ba60d72`) + vfork shared-AS (`8cf6144`): a sibling sharing the L0
   can hold a stale TLB entry for a page already freed and reallocated — not
   caught by the `is_free` guard. The refcount-aware free (part 2) closes the
   single-AS over-free; the cross-thread stale-TLB window may still exist and is a
   candidate for Bug 3. Watch the `DOUBLE-FREE` counter under load.
3. **Decide on commit.** Bug 1 fix (PMM hardening + refcount-aware unmap + 3
   tests) and Bug 2 fix (deferred loader) are self-contained; commit on
   `expand-acceptance` once Bug 3 direction is settled.

## Files touched

Bug 1 — PMM hardening + refcount-aware unmap:
- `src/pmm.rs` — `FreeOutcome`, `DOUBLE_FREE_COUNT`, fixed decrement, accessors.
- `src/main.rs` — `[Mem]` `DOUBLE-FREE=N` marker.
- `crates/akuma-exec/src/mmu/mod.rs` — `remove_user_frame -> bool` (`#[must_use]`);
  `unmap_and_free_page` returns `Some` only on last ref; `Drop` frees once per PA.
- `src/syscall/mem.rs` — eager munmap loops free only when `remove_user_frame` true.
- `src/exceptions.rs` — `let _ =` on CoW `remove_user_frame` (free via `cow_ref_dec`).
- `src/process_tests.rs` — `test_munmap_teardown_conserves_pmm`,
  `test_aliased_pa_not_double_freed` (now `df_delta==0`),
  `test_unmap_and_free_respects_refcount` (new).

Bug 2 — ELF heap-slurp:
- `crates/akuma-exec/src/process/spawn.rs` — size-gated deferred loader (`>1 MiB`
  uses `from_elf_path`).

See also: `docs/COW_OPTIMIZATIONS.md` (the munmap/vfork work), `docs/AI_DEBUGGING.md`
(lldb+gdbstub setup).
