# `Pte` / page-table access newtype in `mmu/mod.rs` (implementation plan)

**Status: Stage A landed 2026-08-16 (§5), Stage B not started.** Written
2026-08-15 as a handoff for implementation; all site counts and line numbers in
§1–§4 were verified against `main` after the `trim-some-more-fat` merge and are
left as written — §5 is the report, appended rather than folded back in, so the
plan and what actually happened stay separable. **Line numbers in §1 are
pre-Stage-A and no longer resolve.** Origin:
[`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md)
§5.1 (its §7 table item 9, "large / **high** risk"), updated for the state of
the file after the 2026-08-14 walk merge
([`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
§8 item 8).

**Goal:** the 73 raw `read_volatile`/`write_volatile` sites in
`crates/akuma-exec/src/mmu/mod.rs` (all in this one file; measured 2026-08-15)
concentrated into a handful of `unsafe` accessors, with PTE flag manipulation
type-checked instead of raw bit arithmetic. Expected: **~−50 `unsafe`**, zero
behavior change.

**This must land as two independently-gated stages.** Stage A is half the
reduction at a quarter of the risk and is worth doing even if Stage B never
happens. Do not start Stage B until Stage A's full gate (§4) is green.

**Read before touching the file:** this is the highest-consequence file in the
repo — the page-table UAF / TTBR-gate work, the CoW/ASID-flush fixes and the
2026-08-15 file-page-cache fixes all live in or route through it. The
invariants are in `docs/reference/subsystems/memory.md` and `.../smp-shared.md`.
Any behavior change here is a bug by definition.

---

## 1. Where the 73 sites are (measured 2026-08-15)

The 2026-08-14 merge (item 8) consolidated the **read-side** walks:
`resolve_user_leaf` (`:1983`, returning `UserLeaf` `:1958`),
`current_user_l3_pte` (`:1617`), and the range walk
`for_each_mapped_user_pte` (`:2064`). What it did **not** touch is the
write-side `&mut self` walks on `AddressSpace`, which is where the biggest
remaining cluster sits:

| cluster | functions (line numbers as of 2026-08-15) | ~sites |
|---|---|---|
| **A. write-side L0→L3 walk, 7 near-identical copies** | `unmap_page_no_flush` `:1093`, `unmap_and_free_page_no_flush` `:1128`, `try_evict_ro_page` `:1179`, `zero_mapped_page` `:1215`, `update_page_flags` `:1242`, `update_page_flags_no_flush` `:1274`, `read_l3_page_entry` `:1303` | ~30 |
| B. mapping cluster | `map_page` `:797`, `get_or_create_table` `:817`, `map_device_page` `:849`, `map_kernel_block_2mb` `:885`, `is_page_mapped` `:935` | ~15 |
| C. boot/setup | `init_shared_device_tables` `:139`-area, `add_kernel_mappings` `:710`, the identity-map extension around `:67`–`:85` | ~8 |
| D. current-TTBR0 helpers (already merged, still raw at the leaf) | `current_user_l3_pte`, `update_current_user_page_flags` `:1592`, `remap_current_user_page` `:1649`, `protect_kernel_code` `:1802` | ~10 |
| E. read-side helpers | `resolve_user_leaf`, `for_each_mapped_user_pte`, `is_page_mapped_ptr` `:2217`, `is_page_user_accessible_ptr` `:2226` | ~10 |

Each copy in cluster A opens with the same four `(va >> n) & 0x1FF` index
extractions and the same descent — `read_volatile`, `& flags::VALID`, mask
`0x0000_FFFF_FFFF_F000`, `phys_to_virt`, next level — and differs only in what
it does at the L3 leaf.

## 2. Stage A — consolidate the write-side walk (~2–3 h coding) — **DONE, see §5**

One private helper on `AddressSpace`, the `&self` analog of what item 8 built
for the current-TTBR0 side:

```rust
/// Walk L0→L2 and return a pointer to the L3 slot for `va`, or None if any
/// intermediate level is unmapped or a block descriptor. Does NOT read the
/// leaf: callers decide what an invalid/valid L3 entry means for them.
/// Takes no IrqGuard and does no TLB maintenance — callers keep their own.
fn l3_slot(&self, va: usize) -> Option<*mut u64>
```

Rules, each of which is a bug-in-waiting if ignored:

- **Check `TABLE` as well as `VALID` at L1/L2.** Several of the seven copies
  check only `VALID`, which would walk a block descriptor's output address as
  a table base — the exact latent bug item 8 found in
  `update_current_user_page_flags` and fixed by `resolve_user_leaf`. Fixing it
  here is correct, but **record it as a found behavioral difference**, don't
  smuggle it in silently.
- **The helper takes no `IrqGuard` and never flushes.** Callers differ on
  purpose: the `_no_flush` variants exist because per-page TLB barriers
  dominated large munmaps (comment at `:1128`); `update_page_flags` flushes
  per page, its `_no_flush` sibling batches. Guard/flush discipline stays at
  the call site, byte for byte.
- **Per-copy leaf semantics differ — table them in the PR.** Some copies
  return `Ok(())` on an unmapped intermediate level, some `None`; some read
  the leaf and check `VALID` separately, some write unconditionally. Expect
  3–4 real behavioral differences across the seven (every clone family so far
  has had them); each needs a recorded decision.
- Keep the helper private and monomorphic. No closures-with-flags API — item 8
  already provides `for_each_mapped_user_pte` for range work; this is the
  single-VA case.

Expected result: cluster A's ~30 sites drop to the handful inside `l3_slot`,
roughly −80 lines, and the file has **one** write-side walk to reason about.
Run the full §4 gate. Land. Only then continue.

## 3. Stage B — the newtype (~3–4 h coding)

Two types in `crates/akuma-exec/src/mmu/types.rs`, next to the existing
`flags` module and `PageTable { entries: [u64; 512] }` (which stays — it is
the *allocation* shape; the new type is the *access* shape):

```rust
/// One AArch64 stage-1 descriptor, by value. Pure bit arithmetic — no
/// volatile, no pointers. All the `& 0x0000_FFFF_FFFF_F000` and
/// `& flags::VALID` spelling in mmu/mod.rs moves here.
#[derive(Clone, Copy, PartialEq)]
pub struct Pte(pub u64);
impl Pte {
    pub fn is_valid(self) -> bool;
    pub fn is_table(self) -> bool;          // VALID + TABLE at L1/L2
    pub fn output_pa(self) -> usize;        // & 0x0000_FFFF_FFFF_F000
    pub fn user_accessible(self) -> bool;   // fold in UserLeaf's logic (:1976)
    pub fn with_user_perms(self, new_flags: u64) -> Pte; // the PERM_MASK swap
    pub const fn empty() -> Pte;
    // constructors mirroring today's inline compositions:
    pub fn page(pa: usize, flags: u64) -> Pte;
    pub fn table(pa: usize) -> Pte;
}

/// A live table at one level. The ONLY place PTE volatile access happens
/// after this stage.
#[derive(Clone, Copy)]
pub struct TableRef(*mut u64);
impl TableRef {
    /// # Safety: pa is a live page-table frame of this address space, not
    /// freed for the duration of use (the TTBR-gate/pending-free rules).
    pub unsafe fn from_phys(pa: usize) -> TableRef;   // phys_to_virt inside
    pub fn get(self, idx: usize) -> Pte;              // read_volatile
    pub fn set(self, idx: usize, pte: Pte);           // write_volatile
}
```

Decisions already made:

- **`Pte` is by-value and never volatile**; only `TableRef::get/set` (and the
  walk helpers built on them) touch memory. This keeps "read once, judge the
  copy" semantics identical — several fixes in this file (the stale-write-fault
  class) depend on exactly when a PTE is re-read, so a conversion must never
  turn one read into two or two into one. **Site-by-site rule: count the
  volatile ops before and after; the count per function must be identical.**
- Index bounds: `(va >> n) & 0x1FF` is in-range by construction. `get`/`set`
  use `debug_assert!(idx < 512)` — **no panicking bounds check on fault
  paths** (a panic reachable from a syscall arm is the exact fat
  `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` warns about).
- `UserLeaf` (`:1958`) either becomes a thin wrapper over `Pte` or its
  accessors delegate; do not leave two copies of `user_accessible`/`phys`
  logic alive.
- Conversion order (small PR-able chunks, compile after each): cluster A's
  `l3_slot` internals → cluster B (`get_or_create_table` is the subtlest: its
  shatter-block branch reads, allocates, then overwrites — keep the exact
  read/write sequence) → cluster D → cluster E → cluster C (boot code last;
  it also touches the boot-TTBR0 teardown documented at the end of
  `TRIM_FAT_EMBARASSING_DUPLICATIONS.md`).
- **Untouched:** `publish_l0_*`/pending-TTBR-free machinery (`:393`–`:613`),
  all `asm!` barriers and `tlbi` sequences, `IrqGuard` placement, the `NG`
  bit and `attr_index` composition in `map_page`. If a flag expression looks
  simplifiable, it is out of scope — convert spelling, not math.
- Host tests: `Pte` is pure — add unit tests for the accessors/constructors in
  `types.rs` (the crate already runs 200+ host tests). Boot-suite additions
  are not required for a behavior-preserving refactor; the existing suite is
  the oracle.

## 4. Verify — the gate for EACH stage

Per [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md),
this is a comparison against a parent-commit baseline, not a green checkmark:

1. **Tiers 1–3**: `scripts/verify_trim.py --out mine.txt`, baseline arm in a
   `git worktree` with `--instance 1`, diff the summaries. Compare failure
   **sets**, not counts; the known-benign table covers `retired_reclaim_ab`,
   the SMP=2 `cowstale` flake, and the rest. The Tier 3 fork/CoW binaries
   (`cowstale`, `bssfork 20 8 1`, `madvshared`, `forkprobe`, `elftest`,
   `stackstress`) are the targeted probes for this file.
2. **Tier 4** (redis memtest): required for Stage B, recommended for Stage A —
   this is a memory-path change by the runbook's own criterion, and the
   memtest is the only sustained-pressure byte-verifying workload in the gate.
3. **Tier 5**: **5-vs-5 self-host clean-build trials** per
   [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)
   § "Run a build trial". `cargo clean` before every trial (no script does it
   for you); baseline is green (10/10 on 2026-08-15), so a single red trial on
   the change arm is a finding — capture logs, match the Common-failures
   table, never resume-retry past it. Check the tripwire greps
   (`[PMM-RESURRECT]`, `[FILL-SHORT] got=Ok(0)`, `defer_leak=`, `[PMM-UAF]`,
   `[PMM-POISON]`, `[WILD-DA]`) **even on green runs** — this refactor's
   failure mode is exactly the wrong/stale/poisoned-frame class those exist
   for.
4. `scripts/build_extreme_size.sh` — the 4.0 MB floor must hold (mmu code is
   in every profile).

Report per the trim-fat lesson: definitions collapsed, **behavioral
differences found between the copies and the decision for each** (Stage A's
per-copy leaf-semantics table), and the volatile-site count:

```bash
grep -c 'read_volatile\|write_volatile' crates/akuma-exec/src/mmu/mod.rs   # 73 before
```

Do not commit — leave the tree for review (repo convention: the user drives
all commits; run clippy + host tests first).

## 5. Stage A — landed 2026-08-16

Baseline for every A/B below: `b8eefbe3` (`trim-even-more-fat`), in a
`git worktree`.

### What was built

One free function, and two callers that supply the L0 from different places:

```rust
unsafe fn l3_slot_in(l0_ptr: *mut u64, va: usize) -> Option<*mut u64>   // the walk
fn AddressSpace::l3_slot(&self, va: usize) -> Option<*mut u64>          // own L0
fn current_user_l3_pte(va: usize) -> Option<*mut u64>                   // live TTBR0
```

This is §2's `l3_slot` with one deviation, and it is a simplification rather
than a scope increase: §2 proposed `l3_slot` as the `&self` *analog* of the
already-merged current-TTBR0 resolver, which would have left **two** copies of
the descent alive. Making the walk a free function over an `l0_ptr` and having
`current_user_l3_pte` delegate to it as well collapses both — so cluster D's
resolver came along with cluster A for free, and there is now exactly one
write-side descent in the file.

Second merge, not called for in §2 but the same clone family:
`update_page_flags` and `update_page_flags_no_flush` were byte-identical apart
from the trailing `flush_tlb_page(va)`, so they now wrap
`update_page_flags_inner(&self, va, new_flags, flush: bool)` — `#[inline]`, with
`flush` a literal at both call sites, exactly the shape `map_user_page_inner`
already uses in this file. The inner takes `&self` and returns `()`; the two
public wrappers keep `&mut self` and `Result`, which is their existing API.
Clippy's `unnecessary_wraps` and `needless_pass_by_ref_mut` (both of which skip
`pub` fns, which is why nobody saw this before) are what surfaced that neither
copy had ever needed `&mut` or been able to return `Err`.

### Numbers

| metric | before | after |
|---|---|---|
| `grep -c 'read_volatile\|write_volatile' mmu/mod.rs` | 73 | **50** |
| `mmu/mod.rs` lines | 2246 | 2211 |
| diff on `mmu/mod.rs` | — | +111 / −146 |
| inline four-index L0→L3 descents | 15 | **8** |

The eight remaining descents are precisely the Stage B worklist and nothing
else: `map_page`, `map_device_page`, `map_kernel_block_2mb`, `is_page_mapped`,
`map_user_page_inner`, `l3_slot_in` itself, `resolve_user_leaf`,
`for_each_mapped_user_pte`.

### Behavioural differences found between the copies, and the decision for each

Seven copies, per §1 cluster A. What each did at the leaf is unchanged — the
helper stops at the L3 *slot* and hands it back, so every per-copy leaf decision
stayed at its call site verbatim:

| # | fn | leaf handling (unchanged) |
|---|---|---|
| 1 | `unmap_page_no_flush` | writes 0 **unconditionally**; never reads the leaf |
| 2 | `unmap_and_free_page_no_flush` | read → `VALID`? → write 0 → return PA |
| 3 | `try_evict_ro_page` | read → `VALID`? → **`AP_RO_ALL` gate** → write 0 → return PA |
| 4 | `zero_mapped_page` | read → `VALID`? → `write_bytes` the *page*, not the PTE |
| 5 | `update_page_flags` | read → `VALID`? → PERM swap → write → **flush** |
| 6 | `update_page_flags_no_flush` | identical to 5 minus the flush |
| 7 | `read_l3_page_entry` | read → `VALID`? → return the descriptor |

**D1 — the `TABLE` test at L1/L2. Real change, and the one that matters.**
§2 predicted this ("several of the seven check only `VALID`"). Measured: **all
seven** checked only `VALID`. Decision: fold the test in, per §2's instruction to
fix it but record it.

It is worse than the latent bug §2 anticipated — it is reachable from EL0.
Every address space has block descriptors, because `add_kernel_mappings`
identity-maps `[ram_base, ram_end)` as EL1-only 2 MB blocks. A VA landing on one
had the block's *output address* taken for an L3 table base, and the walk then
read — or wrote — at `block_pa + ((va >> 12) & 0x1FF) * 8`, which is live kernel
RAM. Both entry points are unguarded:

- `sys_munmap` (`src/syscall/mem.rs`) applies **no** kernel-VA guard at all;
  `mmap_fixed_overlaps_kernel_va` is consulted only by `sys_mmap`. So
  `munmap(ram_base + k*4096, 4096)` reached `unmap_and_free_page_no_flush`,
  which read that qword and, if its bit 0 happened to be set, **wrote 0 over
  it**.
- `sys_mprotect` gates on `aspace.is_mapped(va)` — but that is `is_page_mapped`,
  which *does* test `TABLE` and therefore reports a block as **present**. The
  guard passed, and `update_page_flags_no_flush` then ORed `AP`/`UXN`/`PXN` into
  the same kernel qword.

Post-change both walks stop at the block and do nothing. Regression guard:
`test_kernel_identity_va_walk_stops_at_block` (`src/process_tests.rs`), which
asserts the pair that defines the invariant — `is_mapped(va)` is **true** (a
block is present, deliberately) while `read_l3_page_entry(va)` is **None** — and
then checks the would-be victim qword is byte-unchanged across an
`update_page_flags` + `unmap_page_no_flush`. Its read-only half runs first and
returns early, so a regressed tree reports FAILED rather than reproducing the
corruption it exists to detect.

**D2 — no `VALID`/`TABLE` test at L0.** Not added. With a 4 KiB granule and a
48-bit VA there is no L0 block descriptor, so a valid L0 entry is necessarily a
table; `resolve_user_leaf`, `current_user_l3_pte` and all seven copies already
assumed this.

**D3 — return value on a missing intermediate level** differs per copy (`Ok(())`,
`None`, `false`). `l3_slot` returns `Option` and each caller maps it to its own,
so all three survive.

**D4 — `&mut self` vs `&self`.** Copies 4 and 7 were `&self`, the rest
`&mut self`; none needed `&mut`, since all edit through a raw pointer. The
helper takes `&self`; no public signature changed.

**D5 — `update_page_flags_inner` can only ever succeed.** See above.

### Volatile ops per function — the site-by-site rule

§3's rule ("count the volatile ops before and after; the count per function must
be identical") applies to Stage A too, since several fixes in this file depend on
exactly when a PTE is re-read. Verified by hand:

| fn | before | after |
|---|---|---|
| `unmap_page_no_flush` | 3 R + 1 W | 3 R + 1 W |
| `unmap_and_free_page_no_flush` | 4 R + 1 W | 4 R + 1 W |
| `try_evict_ro_page` | 4 R + 1 W | 4 R + 1 W |
| `zero_mapped_page` | 4 R | 4 R |
| `update_page_flags` | 4 R + 1 W | 4 R + 1 W |
| `update_page_flags_no_flush` | 4 R + 1 W | 4 R + 1 W |
| `read_l3_page_entry` | 4 R | 4 R |
| `current_user_l3_pte` | 3 R | 3 R |

No read became two, and no two became one.

### Gate (§4)

| tier | result |
|---|---|
| 1 — four clippy configs + host tests | all clean; **557/557**, identical on both arms |
| 2 — boot suite SMP=1 and SMP=4 | 95 `[PASS]`, **empty** failure sets, 0 host time-jumps |
| 3 — Tier 3 exercises, both SMP levels | all `ok`; `eager_mprotect` `KNOWN-FAIL (expected)` |
| 4 — redis `--test-memory 512` | `Your memory passed this test`; 0 SIGSEGV, tripwires silent |
| 5 — self-host clean builds, 10 per arm | **IN FLIGHT at time of writing — fill this in** |

The whole Tiers 1–3 A/B diff came to three lines, all accounted for:
`passed_marker` +1 at each SMP level (the new boot test — `pass_marker` stayed 95
because the test emits `[Test] … PASSED`, not `[PASS]`, exactly the two-marker
split the gate runbook warns about), plus one `cowstale` line.

**The `cowstale` line was the baseline arm failing, not this change.** It came up
`UNEXPECTED` at SMP=4 on `b8eefbe3` with the documented stale-write-fault
signature — `[WPF] va=0x420000 cow_ref=0 … ap_rw=true`, `FAR=0x420260
ELR=0x400868 ISS=0x4f`. Since a 1-of-1 result on that probe is noise, it was
A/B'd over five boots per arm: **baseline 4/5, this change 8/8** across two
batches. Same class, same rate; no regression, and *not* enough to claim a fix —
0-in-8 against a ~20 % rate is unremarkable. One first-batch sample scored FAIL
for a harness reason (the probe never ran: sshd accepts later than its boot
marker prints), which is why the harness now scores that INCONCLUSIVE instead —
a missed run must never be indistinguishable from a SIGSEGV.

Tier 5 measured trial cost at **~2m10s**, not the ~7–12 min on record; both
runbooks were corrected, along with the registry-priming step `--offline` needs.

### Not done

Stage B (§3, the `Pte`/`TableRef` newtype) is untouched. §4's instruction stands:
its gate is the full one again, with Tier 4 **required** rather than recommended.

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) §5.1 — the original proposal and the
  "do not do this casually" warning; §7 table item 9.
- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §8 item 8 — the 2026-08-14 walk merge this plan builds on (three clone
  families, one latent block-descriptor bug found), and "Promoted out of
  deferred" for why that merge does not subsume this item.
- [`PAGE_TABLE_UAF_BKL_STORM.md`](PAGE_TABLE_UAF_BKL_STORM.md) /
  [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) — the invariants around frame
  freeing and CoW that make this the highest-consequence file in the repo.
- [`MAPPED_PAGE_PREMATURE_FREE_FIX.md`](MAPPED_PAGE_PREMATURE_FREE_FIX.md) —
  the 2026-08-15 fix whose tripwires double as this plan's Tier 5 detectors.
- [`TRIM_FAT_MMIO_NEWTYPE.md`](TRIM_FAT_MMIO_NEWTYPE.md) — the sibling plan
  for device-register MMIO; deliberately a separate sweep.
