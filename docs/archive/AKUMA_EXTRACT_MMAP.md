# Extracting region bookkeeping: `akuma-mmap`

**Date: 2026-08-29.** Stage 1 of a two-stage split of the memory-syscall family.
Stage 2 (`akuma-syscalls-mem`, the syscall *decision* layer) is not built yet;
§7 records what it would contain and the one question that decides whether it
should exist at all.

This is a companion to [`AKUMA_EXTRACT_SYSCALLS.md`](AKUMA_EXTRACT_SYSCALLS.md)
§8.1/§8.2 (`akuma-syscalls-sync`, `akuma-syscalls-poll`) but is **not** a third
entry in that series. Those two extracted a syscall family's decision logic.
This one extracts a *data structure and its algebra*, which is a different kind
of move with a different justification — see §1.

---

## 1. Why this is not "the mmap syscall extraction"

The obvious job was `src/syscall/mem.rs` (1,437 lines: mmap / munmap / brk /
mremap / mprotect / madvise / msync / membarrier), following the futex and epoll
model. Sizing it first — which is the whole reason step 0 existed — showed the
prize was much smaller than the file, because most of the family's pure logic
had already been extracted:

- `MmapRegion`, `LazyRegion`, `LazySource` and `Process::mmap_regions` were
  already in `akuma-exec`.
- CoW-fork region inheritance was already `inherit_mmap_regions_for_cow_child`,
  already pure over a slice, already host-tested.
- Region splitting/detach was already `detach_eager_regions_in_range`, already
  pure over a `&mut Vec`, already host-tested.
- `prot` → PTE flags was already `akuma_exec::mmu::user_flags::from_prot`.

What was left in the bin crate and genuinely argument-pure came to **five**
functions, ~35 lines: `dontneed_zero_range`, `dontneed_page_action`,
`mmap_fixed_addr_unaligned_einval`, `mmap_fixed_overlaps_kernel_va`,
`membarrier_cmd`. Two more that a first read counts as pure are not:
`dontneed_count_shared` and `dontneed_apply` take a `&UserAddressSpace` and
mutate page tables. `dontneed_apply`'s own doc comment says it was split out
*"so the boot suite can drive it against a real `UserAddressSpace` and a real
CoW-shared frame — the defect is a cross-address-space one, and a test on
either ledger alone cannot see it."* That is a reasoned decision to stay
boot-tested, and it stands.

So a `akuma-syscalls-mem` built on the futex/epoll model would have been a
~65-line crate. Real, but not the biggest thing available.

The biggest thing available was that **the region algebra was already pure,
already tested, and living in the wrong place** — in `akuma-exec`, the crate
that also owns processes, threads, ELF loading, the MMU and the BKL. Nothing
below `akuma-exec` could name a `MmapRegion`, which is what made a future
`akuma-syscalls-mem` impossible to build as a leaf.

---

## 2. The layering decision

Three options were on the table, and the choice had to be made before any code
moved:

| | option | verdict |
|---|---|---|
| (a) | new leaf crate takes already-resolved facts (the `akuma_syscalls_poll::readiness` model), kernel translates | viable but ~65 lines; leaves the region algebra stranded |
| (b) | move `MmapRegion` down to a crate both `akuma-exec` and the new crate depend on | **chosen**, in the form below |
| (c) | no new crate; the work lands in `akuma-exec` beside the region code | least motion; keeps the algebra unreachable from below |

(b) was initially argued *against* on the grounds that `MmapRegion` holds a
`Vec<PhysFrame>` and moving it would drag frame ownership into the boundary.
That objection does not survive reading the definition:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysFrame { pub addr: usize }
```

`Copy`, and **no `Drop` impl anywhere in the tree**. It is a value, not an
ownership token — dropping one frees nothing; ownership lives in
`UserAddressSpace::user_frames` and the PMM's refcounts. A crate can hold a
`Vec<PhysFrame>` with no allocator, no lock and no PMM. `PhysFrame` was also
already defined *above* every one of its users, in `akuma_exec::runtime`, purely
because that is where it landed first; `akuma-pmm` could never name it (it sits
below `akuma-exec` and speaks in raw `usize`).

The shape chosen is therefore two crates, of which only the first exists today:

```
akuma-exec ──► akuma-mmap        (region algebra + PhysFrame + PTE permission vocabulary)
akuma-syscalls-mem ──► ?         (stage 2; see §7 — may need nothing from akuma-mmap)
```

**Why `akuma-mmap` and not `akuma-mem`.** `akuma-mem` sits next to `akuma-pmm`
and would read as its sibling when one is the physical frame allocator and the
other is virtual region bookkeeping. This tree has been bitten by exactly that
before: `akuma-time` was renamed `akuma-syscalls-time` (2026-08-28) so it would
stop reading as a sibling of `akuma-timer`. `-mmap` names what it holds.

---

## 3. What moved, and what deliberately did not

| in `akuma-mmap` | stays in `akuma-exec` |
|---|---|
| `PhysFrame` | `Process::mmap_regions`, `vm_lock`, `vm_with_regions`, `vm_alloc_mmap` |
| `MmapRegion` + constructors, `contains`, `len_bytes`, `frame_for` | `eager_region_flags_for_page_fault`, `eager_regions_containing` |
| `inherit_mmap_regions_for_cow_child` | `update_eager_region_flags`, `munmap_lazy_regions_in_range` |
| `detach_eager_regions_in_range` | `record_mmap_region`, `remove_mmap_region`, `share_rw_range` |
| `PAGE_SIZE`, `PAGE_SHIFT`, `flags`, `user_flags` | `PageTable`, `MAIR_*`, `attr_index`, `ENTRIES_PER_TABLE`, block sizes |
| 19 host tests | `FaultAccess`, `lazy_map_flags` (demand-paging policy) |

Every line in the right column is there for a stated reason, not for
convenience:

- **`vm_lock` / `vm_with_regions`** exist to close a `CLONE_VM` data race. That
  is a locking argument, not region algebra. `akuma-mmap` has **no
  dependencies at all**, so it cannot lock — the property is enforced by the
  dependency list rather than by reviewer discipline.
- **The pid-keyed accessors** each resolve a process before they can reach a
  region list. A crate that cannot name a `Process` cannot host them.
- **`FaultAccess` / `lazy_map_flags`** are demand-paging *policy* — the seam
  between the data-abort and instruction-abort arms of the EL0 handler
  ([`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §12). Policy belongs with the fault
  path it serves, not in a leaf everything depends on.
- **`PageTable`, `MAIR_*`, `attr_index`** are page-table structure and memory
  attributes. No region ever names them.
- **The fault path, the PMM, TLB maintenance and ASID handling** never came up
  as candidates and must not: `docs/archive/` records a TLB/ASID bug (`tlbi
  vale1is` never matching a user ASID) and a `sys_munmap` use-after-free that
  fired ~11,000 times per self-host build. Both are effects with hardware
  arguments attached.

`user_flags` and `PAGE_SIZE` moved because they had to: a region's `flags` field
*is* a `user_flags` value and `detach_eager_regions_in_range` divides by
`PAGE_SIZE`, so both had to sit at or below the crate owning the region. This is
the same move, for the same reason, as the device-window table that earlier went
from `mmu/types.rs` down to `akuma-primitives` so `akuma-virtio` could reach
`DEV_VIRTIO_VA` without depending on `akuma-exec`.

**Zero call sites changed.** `akuma_exec::runtime`, `akuma_exec::mmu::types` and
`akuma_exec::process::{types,children}` re-export everything that moved, so the
45 `PhysFrame` uses in `mmu/mod.rs`, the 57 `MmapRegion` uses across the tree,
and every `crate::mmu::user_flags::*` path resolve exactly as before. The move
is invisible above the boundary, which is what made it reviewable.

---

## 4. What the move found

**Two duplicated tests.** Copying `user_flags`' tests into the new crate while
leaving `mmu/types.rs`'s test module intact produced two tests asserting the
same thing in two crates. The workspace test count caught it — the arithmetic
`958 − 14 + 19 = 963` did not match the expected `961`, and the missing 2 were
the duplicates. Deleted from `akuma-exec`; the count then closed exactly. This
is the deduplication half of the job that step 0 predicted would exist, and the
lesson is that **a test-count delta is a checkable quantity** — reconcile it
rather than eyeballing "still green".

**`akuma-exec`'s allow block was hiding four lint classes.** The moved code was
clippy-clean in `akuma-exec` and dirty the moment it landed in a crate without
`akuma-exec/src/lib.rs`'s 30-entry `#![allow(...)]`:

| lint | sites | what it was hiding |
|---|---|---|
| `clippy::must_use_candidate` | 11 | discarding `MmapRegion::owned(...)` — a pure constructor — is silent |
| `clippy::assert_is_empty` | 2 | `assert!(x.is_empty())` prints no contents when it fails |
| `clippy::ptr_arg` | 1 | a test helper took `&Vec<MmapRegion>` where `&[MmapRegion]` works |
| `clippy::too_long_first_doc_paragraph` | 2 | rustdoc summary line ran long |

All four were fixed rather than allowed, matching `akuma-syscalls-poll` and
`akuma-syscalls-sync`, which carry **no crate-level allows** and write
`#[must_use]` per function (19 of them in poll). A new leaf with no allow block
is what makes it their sibling rather than `akuma-exec`'s. None of the fixes
changes behaviour: eleven attributes, one slice parameter in test code, two
assertion forms in test code, and one doc-comment paragraph break.

---

## 5. Verification

All gates run at the point of writing; nothing was committed.

| gate | result |
|---|---|
| `cargo test --target $HOST` (workspace) | **961 passed, 0 failed** (was 958 at 18c60d1a: −16 moved out of `akuma-exec`, +19 in `akuma-mmap`) |
| `cargo clippy -p akuma-mmap --target $HOST --all-targets -- -D warnings` | clean |
| `cargo clippy --release -- -D warnings` | clean |
| `cargo clippy --release --no-default-features --features devbox,sound,no-tests,rump-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll -- -D warnings` (rump devbox, no-smoltcp) | clean |
| `cargo clippy --profile extreme-size --no-default-features --features no-tests,smoltcp,extreme,userspace-sshd -- -D warnings` | clean |
| `cargo build --release` | clean |
| `bash .git/hooks/pre-commit` | **exit 0** |
| `#![forbid(unsafe_code)]` | in `crates/akuma-mmap/src/lib.rs`; row added to [`crate-safety.md`](../reference/crate-safety.md) |

**No in-guest run was needed and none was done.** This stage moved no logic: every
moved function is byte-identical apart from four `crate::mmu::` → `crate::` path
rewrites, and the kernel reaches all of it through re-exports. The behavioural
surface is unchanged by construction, which is why the gate list stops at the
build. Stage 2, which *would* change how a syscall decides, does not get that
exemption — it needs `scripts/mem_suite.py` and the A/B/A cost run described in
§7.

---

## 6. Line-count effect, and the comment-ratio question

Measured with `scripts/cloc_akuma.py src crates`:

| point | comment / code |
|---|---|
| 2026-08-23 (as recorded in [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) Stat 3) | 44.1% |
| 18c60d1a — immediately before this change | 47.6% |
| after `akuma-mmap` | 47.7% |

**This change accounts for +0.1 of the +3.6.** The rest predates it: the three
syscall-family extractions that landed between 2026-08-23 and 18c60d1a. The
mechanism is visible per component:

| component | code | comment | ratio |
|---|---|---|---|
| `src` | 30,887 | 14,022 | 45.4% |
| `crates/akuma-exec` | 16,083 | 9,900 | 61.6% |
| `crates/akuma-syscalls-poll` | 740 | 657 | **88.8%** |
| `crates/akuma-syscalls-sync` | 741 | 517 | 69.8% |
| `crates/akuma-mmap` | 398 | 266 | 66.8% |

Every extraction moves code from `src/` (45.4%) into a leaf that documents its
own seam (67–89%), and the extraction convention — "what must not move, and
why" — is prose that did not exist before the code was extracted. So the ratio
climbing is a *direct mechanical consequence* of the extraction programme, not
an independent trend.

That cuts both ways, and Stat 3's two readings both still apply. Reading A (the
commentary records why an invariant exists, citing the investigation that
established it) is clearly what `akuma-syscalls-poll`'s 88.8% is. Reading B
(code needing that much explanation may have invariants its structure cannot
express) is the one to watch: a seam that needs 657 lines of prose to say what
must not cross it is a seam a reader cannot infer from the types. `akuma-mmap`
is the mild case at 66.8% — its central claim ("this crate has no dependencies,
so it cannot lock or allocate") *is* expressed structurally, in the empty
`[dependencies]` table, and the prose only points at it.

---

## 7. Stage 2: `akuma-syscalls-mem`, and the question that decides it

Not built. What it would hold, all currently in `src/syscall/mem.rs`:

- the five argument-pure seam functions listed in §1;
- `sys_mmap`'s mapping-kind classification (`mem.rs:441-497`) — `is_lazy`,
  `is_file_backed`, `is_shared_writable`, `use_lazy`, pure over
  `(prot, flags, fd >= 0, pages, MMAP_EAGER_MAX_PAGES)`;
- `sys_mremap`'s shrink short-circuit and its `MREMAP_MAYMOVE`-absent
  `ENOMEM`-vs-`EFAULT` split, pure over
  `(old_addr, old_size, new_size, flags, va_limit, is_mapped)` where
  `is_mapped` is a probed bool — the direct analogue of
  `akuma_syscalls_poll::readiness::FdState`.

**The question: does it need `akuma-mmap`?** The evidence says no. The
classification reads *raw `prot` and `MAP_*` bits*, which already live in
`akuma-syscalls-linux`; `from_prot`'s result is computed alongside and handed to
`push_lazy_region`, which is a kernel-side effect. `KERNEL_VA_START` is
`ProcessMemory::KERNEL_VA_START` in `process/types.rs`, not in the MMU, and
`kernel_va_end()` is RAM-dependent at runtime — both become parameters. If that
holds, stage 2 is a leaf on `{akuma-syscalls-linux, akuma-primitives}`, the
identical dependency set as `-sync` and `-poll`.

Treat it as a **check, not an assumption**. If stage 2 finds itself reaching for
`MmapRegion`, the seam is drawn in the wrong place and the fix is to move the
seam, not to add the dependency.

Constraints stage 2 inherits and this stage did not have to satisfy:

- **Behaviour-preserving, strictly.** A divergence from Linux found along the
  way gets preserved, pinned with a test named to say what it is, and documented
  under "Known divergences" in
  [`syscalls/mem.md`](../reference/subsystems/syscalls/mem.md). One is already
  known and currently unpinned: `dontneed_zero_range` rounds an unaligned start
  **down**, so `MADV_DONTNEED` clears a strict superset of what Linux clears —
  including the caller's partial head page, whose live bytes Linux never
  touches. Its doc comment states this; no test asserts it.
- **Argument validation must stay before `lookup_process`.** `sys_mmap:428-434`
  checks `mmap_fixed_addr_unaligned_einval` before resolving the process
  precisely so a kernel-test caller with no current process gets `EINVAL` rather
  than `ESRCH`. `test_mmap_einval_through_handle_syscall` is the only coverage of
  that ordering and **cannot move to a host crate** — the property is about
  where the call sits in the entry point, not about the predicate. Moving the
  predicate's test out and deleting this one would silently drop it.
- **Gates stage 2 owes that stage 1 did not**: `scripts/mem_suite.py` (no runner
  exists for the ten probes already in `userspace/forktest/c_stress/` —
  `mmap_stress`, `mmapsum`, `mmap_file`, `mprotectlb`, `mremapmove`,
  `madvshared`, `shmanon`, `cowstale`, `eager_mprotect_probe`, `smapsdirty`),
  and an A/B/A cost run via `userspace/memprobe/`. Unlike epoll, mmap/munmap sit
  on the fault path, so that gate has a real chance of saying something.

---

## 8. Boot tests: what stage 2 should and should not delete

`src/tests.rs` + `src/process_tests.rs` carry **77** memory-syscall tests (a
wider grep than the 46 originally estimated). They split three ways, and the
third pile is a decision rather than a sort:

1. **Pure arithmetic — movable.** `test_lazy_region_munmap_prefix/suffix/
   middle/multi`, `test_eager_munmap_*`,
   `test_mmap_fixed_addr_unaligned_einval_helper`,
   `test_membarrier_query_returns_bitmask`, the `alloc_mmap` kernel-hole tests.
2. **Needs a real address space — stays.** `test_madvise_dontneed_frees_pages`,
   `test_lazy_munmap_frees_demand_paged_frames`,
   `test_munmap_fallback_clears_stale_ptes`,
   `test_mprotect_flag_update_with_cache_maintenance`,
   `test_mremap_lazy_region_moves_data`, `test_clone_vm_mmap_regions_on_owner`.
3. **Tests the entry point's ordering — must not move.**
   `test_mmap_einval_through_handle_syscall`, per §7.

Pile 1 overlaps `akuma-mmap`'s `detach_middle_splits_into_two_survivors`,
`detach_cow_inherited_region_splits_by_pages` and
`mprotect_splitting_a_region_keeps_the_pin`. Note the measurement trap when
sizing the result: deleting boot-suite tests shrinks the image because the
*tests* left, not because the code got smaller. Report the two separately or the
number means nothing — and compare with `--features no-tests` to hold the suite
constant.

---

## 9. Tooling: `cloc_akuma.py` now counts `unsafe` and enforced-safe code

Added while writing §6, because [`crate-safety.md`](../reference/crate-safety.md)
turned out to be carrying two hand-maintained numbers that had gone stale: its
prose said "Ten of the eighteen extracted crates" while its own two tables listed
12 and 9. Numbers a document cannot regenerate are numbers that drift.

`scripts/cloc_akuma.py src crates` now prints an **Unsafe by crate** section:
per-crate `unsafe` sites, sites per kloc, a `forbid` marker, and the aggregate
"code in enforced-safe crates". The same fields are in `--json`
(`unsafe_sites`, `forbids_unsafe`, per component and per file) and work under
`--rev`, so a claim can be checked against any commit.

**It counts tokens, not grep hits.** The counter hooks the identifier branch of
the existing lexer, which string literals, comments, char literals and `asm!`
bodies never reach — so a mention of "unsafe" in a doc comment is not a site.
`unsafe_code` (as in `#![forbid(unsafe_code)]`) lexes as one identifier and is
not counted; `#[unsafe(no_mangle)]` is.

Validation, and it is a strong one: every crate carrying
`#![forbid(unsafe_code)]` reports **0 sites** despite several containing the word
in prose — and `forbid` makes a real `unsafe` there a hard compile error, so 0 is
independently known to be the right answer. The script says so itself if the
count is ever non-zero.

What it found:

- `akuma-exec`'s documented "~216" was a `grep -c`, which counts *lines
  containing* `unsafe`. 232 lines mention it; 221 are real sites.
- **`src/` was never counted by that document at all** — 44,160 lines and **313
  `unsafe` sites, 46% of the tree's 680**. The "367" figure everyone had been
  reading was the `crates/` subtotal, not the kernel's.
- The honest enforced-safe fraction is therefore **11.3% of the first-party tree**
  (10.6% of production code), not the 23.3% that `crates/`-only measurement
  suggests.

`--rev` also makes this stage's effect checkable in one command: 18c60d1a reports
12 of 21 crates and 9,225 safe lines, the working tree 13 of 22 and 9,623 — with
`unsafe` sites unchanged at 367, confirming the move neither added nor removed
any.

---

## Background

- [`AKUMA_EXTRACT_SYSCALLS.md`](AKUMA_EXTRACT_SYSCALLS.md) §8.1, §8.2 — the
  `akuma-syscalls-sync` and `akuma-syscalls-poll` extractions this one is
  modelled on and deliberately differs from.
- [`CARGO_HEAP_NULL_RC.md`](CARGO_HEAP_NULL_RC.md) D8/D9 — why
  `detach_eager_regions_in_range` clips rather than matching one region.
- [`FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`](FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md)
  — why `MmapRegion::pages` carries extent independently of frame ownership.
- [`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
  §3 — why `MmapRegion::flags` exists and why its default is `NONE`.
- [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §12 — the `FaultAccess`/
  `lazy_map_flags` policy table that stayed in `akuma-exec`.
- [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) Stat 3 — the comment-ratio
  reading this doc's §6 re-measures.
- [`reference/subsystems/memory.md`](../reference/subsystems/memory.md) — owns
  CoW fork, lazy regions, the mmap bump allocator and eviction; unchanged by
  this move.
