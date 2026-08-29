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

## 10. Stage 2 landed: `akuma-syscalls-mem`

Built 2026-08-29, on top of stage 1 and answering §7's open question in the
affirmative: **it needs nothing from `akuma-mmap`.** Its dependency list is
`{akuma-syscalls-linux, akuma-primitives}` — byte-identical to `akuma-syscalls-sync`
and `akuma-syscalls-poll`. The mapping-kind decision reads raw `prot`/`MAP_*` bits,
which the ABI crate already owns; it never sees a region. That is the seam holding.

439 lines, 37 host tests, `#![forbid(unsafe_code)]`, no crate-level allow block.

| module | holds |
|---|---|
| `mmap` | `plan()` (the mapping-kind decision), `MAP_FIXED` validation, `munmap_len` |
| `mremap` | `plan()` (argument errors, shrink short-circuit, grow) and `no_move_errno` |
| `madvise` | `action()` (advice decode), `dontneed_zero_range`, `dontneed_page_action` |
| `membarrier` | `command()` decode; the `dsb ish; isb` stays in the kernel |

**The gated probe was preserved.** `mremap::Plan::Grow` reports `may_move` rather
than deciding, so the kernel runs the "is `old_addr` mapped?" probe only when
`MREMAP_MAYMOVE` is absent — three lookups and a `vm_lock` acquisition that would
otherwise be paid on every growing `mremap`. This is the exact trap the epoll
extraction fell into on its first draft; a host test
(`may_move_is_reported_so_the_probe_stays_gated`) pins it.

Boot tests removed as subsumed: `test_membarrier_query_returns_bitmask`,
`test_mmap_fixed_addr_unaligned_einval_helper`,
`test_madvise_dontneed_range_semantics`. Kept deliberately:
`test_membarrier_private_expedited_succeeds` (executes a real barrier) and
`test_mmap_einval_through_handle_syscall` (asserts validation runs *before*
`lookup_process`, a property of where the call sits — see §7).

Workspace host tests: 961 → **998**.

### 10.1 What the extraction found: two reachable overflow defects — FIXED

Host tests run with overflow checks on; the kernel ships `--release`, where the
same expressions wrap silently. Two of the moved functions failed immediately.

**Defect A — `madvise` unbounded loop.** `dontneed_zero_range`'s
`(addr.saturating_add(len) + 0xFFF) & !0xFFF` guarded the first addition but not the
rounding. For `len` near `usize::MAX`, `end` wrapped to 0, `end - start` underflowed,
and the page count came out **4,503,599,627,370,495** (~4.5e15).
`syscall/mod.rs` passes `len` straight from a user register with no validation, and
`madvise_dontneed_range`'s pass 0 then runs
`(0..pages).map(..).filter(..).collect()` — a lazy-region lookup per page, inside an
`MmBklGuard` window. `madvise(addr, -1, MADV_DONTNEED)` from unprivileged userspace
was an unbounded kernel loop.

**Defect B — `MAP_FIXED` kernel-VA guard bypass.** `fixed_overlaps_kernel_va`'s
`pages * 4096` overflowed for the same class of `len`, wrapping `map_end` back down
to `addr`, so the guard answered "no overlap" for a mapping spanning the whole
address space — including the kernel identity map the guard exists to protect. In
practice `sys_mmap` then hung in `for i in 0..pages { aspace.unmap_page(va) }` before
it could corrupt anything, so it presented as a hang rather than a compromise.

**Fixed 2026-08-29.** Saturating arithmetic alone is *not* the fix — a correctly
saturated 4.5e15-page range is still 4.5e15 pages to walk. The fix is input
validation at the syscall boundary, which neither handler had:

| | |
|---|---|
| `mmap::len_too_large(len, va_limit)` | `sys_mmap` returns `ENOMEM` for a length exceeding the user address space, **before** it becomes a page count |
| `madvise::range_fits_user_va(addr, len, va_limit)` | `sys_madvise` returns `EINVAL` for a range that escapes the address space or overflows `usize`, before the advice decode and before any process lookup |

The arithmetic was made saturating as well, so the page count is monotonic in `len`
and `end` can never fall below `start` even if a future caller skips the guard.

Verification:

- Crate: 41 host tests, including `huge_len_saturates_instead_of_wrapping`,
  `range_that_overflows_usize_is_rejected`,
  `huge_len_still_reports_the_kernel_va_overlap`, and
  `saturation_does_not_make_the_guard_unconditional` (the fix must not turn the
  overlap guard into "always true").
- Boot suite: `test_mmap_madvise_hostile_length_is_refused` drives all four hostile
  shapes plus one legitimate call through `handle_syscall`. **Its value is that it
  returns at all** — pre-fix it would hang the boot suite rather than fail it.
- Both platforms: QEMU and Firecracker-under-Lima, `PASSED=307 FAILED=0 POISON=0`.

That these sat in a bin crate for months and fell out within a minute of the same
code compiling under `cargo test` is the clearest argument for the extraction
programme this document contains.

### 10.1.1 Why `--release` did not catch them: overflow checks, measured

`overflow-checks` defaults to `false` in Cargo's release profile and nothing in this
tree overrides it. **Bounds checks are unaffected** — those are a language-level
guarantee present in every profile, confirmed by the `index out of bounds` panic
strings in the shipped `release` (4), `extreme-size` (3) and `devbox` (3) binaries.
Only integer overflow checking is off.

Measured cost of turning it on (`aarch64-linux-musl-size -A`, 2026-08-29):

| profile | `.text` | `.rodata` | total |
|---|---|---|---|
| `release` (opt 3, thin LTO) | 2,875,040 → 2,589,664 (**−9.9%**) | 434,408 → 465,912 | 4,551,338 → 4,297,706 (**−5.6%**) |
| `extreme-size` (opt z, fat LTO) | 538,228 → 554,604 (**+3.0%**) | 75,968 → 93,648 (+23%) | 1,018,294 → 1,052,350 (**+3.3%**) |

**The direction reverses by profile**, which is the useful part. At `opt-level = 3`
the extra panic branches raise LLVM's inline cost estimate and suppress inlining
that was bloating `.text` — the checks pay for themselves and then some. At
`opt-level = "z"` there is no inlining bloat to suppress, so the cost is what you
expect: more code, and 23% more `.rodata` for the panic strings.

Three caveats before anyone enables it:

1. **Smaller `.text` is not faster.** Less inlining may well cost more at runtime
   than the checks save in I-cache. Unmeasured — see §10.3.
2. **With `panic = "abort"`, an overflow check in a syscall path turns a silent
   wrap into a kernel abort.** For arithmetic on user-supplied input that is a
   userspace-triggerable kernel panic: a crash instead of a hang, not a fix.
3. It is therefore a **detection** tool, not a mitigation. The defects above are
   fixed by validating input, which is what §10.1 did. The natural home for
   `overflow-checks = true` is a test/CI profile where an abort is a loud signal,
   not the shipped kernel.

### 10.3 Performance: measured twice, no effect either time

**Run 2 (2026-08-29, idle host) is the definitive one.** Five boots at SMP=4,
`mem_op_cost 100 500`: three of the working tree (A, extraction + length guards +
the `mprotect` fix) against two of `f49ca08f` (B, everything but the `mprotect`
fix). The `getpid` control read **134 ns on all five boots — 0.0% spread**, which
is the cleanest control this tree has produced and is what makes the rest
readable.

| arm | A med | B med | delta | A-to-A spread |
|---|---:|---:|---:|---:|
| `mmap_einval` | 1.00 | 1.00 | **0.0%** | 1.5% |
| `munmap_noent` | 3.04 | 3.05 | −0.2% | 26.9% |
| `mprotect_noop` | 2.82 | 2.81 | +0.3% | 22.8% |
| `madv_unmapped` | 13.70 | 13.57 | +0.9% | 1.8% |
| `membarrier` | 0.99 | 0.99 | −0.8% | 3.0% |
| `brk_query` | 1.28 | 1.29 | −0.6% | 2.3% |
| `mmap_enomem` | 1.00 | 1.00 | **0.0%** | 3.0% |
| `madv_einval` | 0.99 | 0.99 | −0.8% | 4.5% |

Every delta is ≤0.9%, against a control that did not move at all. **No effect.**

Run 1 (an earlier, noisier boot set against `e848fbe8`) agreed but resolved less:
control spread 5.1%, deltas ≤3.1%, and `munmap_noent` sat exactly at its own
resolution limit. Recorded because the *disagreement between the two runs is
itself the method's point*: run 1's apparent +2.7% on `munmap_noent` vanished at
0.0% control spread, so it was drift, not code.

#### The `mprotect` gate on the fault path

§10.4's fix adds a region lookup (`eager_region_flags_for_page_fault`, which takes
`vm_lock`) to the EL0 write-fault path. Lock order forbids moving it inside the
`as_lock` hold where `cow_ref` is already known, so it runs on every
write-permission fault that survives `stale_write_fault_absorbed` — which is a
real design concern, not a hypothetical, and none of the arms above touch it.

`userspace/memprobe/c/cow_fault_cost.c` (new) measures it: a child faults N
pages its parent has already made resident, and two page counts bracket the cycle
so `fork` and `exit` cancel — `per-fault = (cow_512p − cow_1p) / 511`.

| | per CoW write fault |
|---|---|
| A (with the gate) | 1236 / 1099 / 1311 ns — median **1236** |
| B (without) | 1387 / 1129 ns — median **1258** |
| delta | **−22 ns (−1.7%)**, against a 212 ns A-to-A spread |

Inside the noise, and nominally negative. A CoW fault costs ~1.2 µs; one region
lookup does not show up in that. **So the placement stands** — the measurement is
what says not to optimise it, and the lock-order argument is what says not to move
it. Do not "fix" this without re-running the probe.

### 10.3.1 The arms that were missing, and two probe bugs they exposed

The first probe pass measured the decode paths and stopped there. Widening it
(2026-08-29) closed four gaps, every one of them inside the extracted crates'
blast radius:

| gap | arm added | measured |
|---|---|---|
| `mremap` had **no arm at all**, despite being in the crate | `mremap_inplace`, `mremap_efault` | 132 ns / see below |
| `MADV_WILLNEED` — the other implemented advice | `madv_willneed` | 176 ns (floor+28) |
| `brk` growth allocates and maps per page; only `brk(0)` was timed | `brk_noop`, `brk_grow_*` | 182 ns / 299 ns per page |
| demand paging is a *translation* fault, a different path from CoW's *permission* fault | `demand_1p`/`demand_512p` | **798 ns** per fault |
| `plan()`'s central lazy-vs-eager decision — its two outcomes were never priced | `mmap_lazy`/`mmap_eager` | 859 vs 1752 ns, **893 ns** eager premium |

For reference, `per_cow_fault` is **2213 ns** — about 2.8x a demand fault, which
is what copying 4 KiB buys you.

**Probe bug 1: `mmap_lazy`/`mmap_eager` were quantisation, not measurement.**
One mmap+munmap cycle is ~1 µs and `clock_gettime` truncates to microseconds
here, so the arms read exactly 1000 and 2000 ns and produced a beautiful "ratio
2.00" that was two clock ticks. Fixed with a 1000-rep inner loop; the honest
ratio is 2.04. This is method warning 3 from `mem_op_cost.c` biting the file
that documents it.

**Probe bug 2: `brk_grow` reported 0 ns for growing 2 MB.** The arm shrank back
with `brk(base)` after each pass to keep passes comparable — but a brk shrink
does **not** unmap. Pass 2 found every page already mapped, `set_brk`'s
allocation loop was skipped, and `best_of` takes the *minimum*, so it reported
the warm pass. An arm that is not idempotent cannot be minimised over. Now grows
monotonically, so every pass allocates fresh.

Both were caught by the numbers looking wrong, not by a test. That is the case
for reading a probe's output rather than diffing it.

### 10.3.2 A finding the new arms surfaced: EFAULT cost 250 µs — FIXED

`mremap_efault` read **~250 µs, ~1600x the syscall floor**, for what is argument
decode and nothing else. It was not the decode: `config::SYSCALL_ERRNO_DIAG_ENABLED`
was `true` by default, and the epilogue wrote a ~103-byte `[EFAULT] …` line to the
serial console on every EFAULT-returning syscall. At ~2,400 ns per byte — one
trapping MMIO store each — that made the trace **99.94%** of the call, on a path
userspace controls.

The gate has been turned **off** (2026-08-29), taking the arm from 249,806 ns to
**150 ns**, and a probe run's boot-log contribution from ~160,000 lines to 55.

The full investigation — the per-byte cost model and its three-build fit, the
dead `mmap`-EINVAL decode the EFAULT-only narrowing had silently created, the
options weighed (including a rate limiter, built and then dropped as unnecessary
complexity), and an audit of every other default-on console trace — is its own
document: [`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md). It is a logging finding,
not a memory one; it is recorded there so it is findable by the next person
measuring a syscall, not buried in an extraction writeup.

### 10.4 The correctness gate, and what it found

`scripts/mem_suite.py` (new) runs the ten probes already in
`userspace/forktest/c_stress/`, which had no runner. Its verdict is layered
because the ten are heterogeneous: **output must exist** (a silent probe is never
a pass), non-zero exit fails, any `FAIL` word fails, and `DIVERGE` is reported
without failing. Result on the fixed tree: **9/10, 3 DIVERGE**.

It found one real bug immediately, unrelated to this extraction and present on
`main`:

**`mprotect` was defeated by a fork.** `eager_mprotect_probe` failed on `main`,
on `08d4c805` and on this branch: a child writing to a page its parent had
`mprotect(PROT_READ)`-ed did **not** SIGSEGV. Root cause: the EL0 write-fault
handler's CoW-break arm fires on `cow_ref > 0` alone and hands the writer a
private **writable** copy. A CoW-demoted page and an `mprotect(PROT_READ)` page
are both read-only in the PTE and indistinguishable from hardware state — which is
precisely what `MmapRegion::flags` was added to disambiguate — and the CoW arm
never consulted it. It also runs *before* the eager-region gate, whose
`AP_RW_ALL` check was therefore never reached on a forked child.

Fixed by gating all three repair paths on the recorded protection:

- New `akuma_mmap::user_flags::is_write` — the predicate that was missing, with
  host tests pairing it against `from_prot` across the whole table.
- New `recorded_protection` / `write_allowed_by_record` in `exceptions.rs`.
  **`None` means "no record", not "not writable"** — an ELF `.data`/`.bss` page has
  no region at all, and a lazy region that recorded nothing carries `flags == 0`;
  both must keep the historical path or `cowstale` breaks.
- The CoW break now declines when a record says the mapping is not writable.
- The lazy-upgrade arm's gate was `!is_none(region_flags)`, which admits
  `PROT_READ` — the same hole on lazy regions. Now `is_write`, and it installs
  the region's *recorded* flags instead of a hardcoded `RW_NO_EXEC`.

Verified: probe PASSes; boot suite 0 FAILED / 0 POISON; the new `[MPROTECT-DENY]`
trace fired 6 times, all at the probe's own addresses in phase pairs — the
intended SIGSEGVs, no false positives.

`cowstale` remains red and is **not** this change's doing: it fails on `main`
(2 of 4 runs SEGV), it is load-driven, and `[MPROTECT-DENY]` fired **zero** times
across four `cowstale` runs on the fixed kernel, which exonerates the new gate
directly. Known issue, tracked elsewhere.

`smapsdirty` was red for a different reason: it is Linux-calibrated, and three of
its four checks test things Akuma deliberately does not do. Its own comment for
`MADV_FREE` had gone stale — it claimed Akuma returns 0, when returning `EINVAL`
is the intended behaviour and the reason Redis starts. The probe now has a third
outcome and reports those three as DIVERGE, testing for the *specific* expected
deviation so an undocumented change still fails.

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
