# Newtype audit: typed addresses and swapped-parameter risk

**Date:** 2026-11-05. **Scope:** the mm-domain (`akuma-primitives/src/addr.rs`,
`akuma-pmm`, `akuma-mmu`, `akuma-mmap`, `akuma-phys`, `akuma-pagetable`,
`akuma-pte-{amd64,aarch64}`, `akuma-user-layout`, `akuma-exec/src/pmm.rs`,
`amd64/src`) plus syscall glue (`akuma-syscalls-glue/src`); production code
only (excludes tests, `userspace/`).
**Status:** survey complete; **no types landed; recommendations only**.
Groundwork for deciding the question "should we introduce a bunch of new
types to delineate params and ban math that doesn't make sense".

## Why

The address vocabulary is raw `usize` with conventions:
`crates/akuma-primitives/src/addr.rs:144` `virt_to_phys(vaddr: usize) -> usize`,
`:167` `phys_to_virt(paddr: usize) -> *mut u8`, `:179` `mmio_phys_to_virt`.
No `PhysAddr`/`VirtAddr`/`Pfn` type exists anywhere in the tree. The repo's
newtype doctrine (`TRIM_FAT_MMIO_NEWTYPE.md`, `TRIM_FAT_PTE_NEWTYPE.md`): a
newtype pays only when construction is rarer than use AND it concentrates an
invariant/unsafe-vouch checked once; net-zero conversions exist. This audit
measures where those conditions hold, ranked by bug history.

## Method

Three greps over the mm-domain: (a) address-named `usize` fn params
(`(va|vaddr|pa|paddr|pfn|addr|base)*: usize`), (b) glue fns with ≥3 positional
`usize` params, (c) page/byte math operators (`>> 12`, `& !0x…`, `PAGE_SIZE`,
`PAGE_SHIFT`). Context reads for ambiguity; bug-doc skim for ranking.

## Findings

### A. Raw-usize address params: **248**

| component | sites |
|---|---:|
| `akuma-mmu/src/lib.rs` | 112 |
| `akuma-pmm/src/lib.rs` | 52 |
| `amd64/src/mm.rs` | 23 |
| `akuma-mmap/src/{region,span,frame}.rs` | 22 |
| `akuma-pagetable/src/{amd64,aarch64}.rs` | 10 |
| `addr.rs`, `akuma-pmm/cow_table.rs`, `akuma-exec/src/pmm.rs`, `amd64/shmpages.rs` | 20 |
| remainder | 9 |

The ambiguity is name-only convention. `akuma-pmm/src/lib.rs`: `init(base,
size, kernel_end)` `:512`, `add_ram(base, len)` `:574`, `reserve(base, len)`
`:595` — bytes or page frames? Correct only by reading the body. Note the
host-build reality: phys↔virt translation on amd64 is currently the identity
(`addr.rs:417` test `translation_is_the_identity`), which is exactly what makes
VA/PA/PFN confusion cheap to write and invisible to tests. A newtype here buys
*distinctness*, not conversion.

### B. Swap-risk positional params in syscall glue: **5 fns**

`syscalls-glue/src/mem.rs` 4, `signal.rs` 1; `fs.rs` and `net.rs` zero. Small:
the API-shape fix list at the boundary is short and cheap. No incident doc
attributes a historical bug to a swapped glue param.

### C. Page/byte math sites: **311**

`akuma-mmu/lib.rs` 86, `amd64/mm.rs` 57, `akuma-pmm/lib.rs` 51,
`akuma-mmap/span.rs` 38, `region.rs` 15, `amd64/loader.rs` 13, `akuma-phys` 10,
`amd64/idt.rs` 7. The `loader.rs`/`idt.rs` hits are descriptor-table
arithmetic, not address math — excluded from the recommendation.

### D. Bug-history ranking

1. **VA/PA/PFN confusion** — `AKUMA_AMD64_4B_FLIP.md`,
   `AKUMA_AMD64_PHYSMAP_ABOVE_4GIB.md`, `TTBR0_AND_THREADING_FIXES.md`,
   `MPROTECT_TLB_ASID_BUG.md`; plus VEC_AUDIT's EL0-reachable
   block-descriptor-as-table-base corruption (found by consolidation work,
   not typing — rigor pays in found bugs).
2. **Unit-mixed byte/page math** — `GO_BINARY_VA_SPACE.md` class (layout
   arithmetic gone wrong).
3. **Swapped glue params** — no known incident; lowest priority.

## Recommendation

1. **First axis: `PhysAddr`/`VirtAddr`/`Pfn` in `akuma-primitives`** (the
   dependency-free leaf), `#[repr(transparent)]`, operator-minimal: `PA + PA`
   does not compile; `PA ± delta` does; `PA − PA -> delta`; `pfn ↔ pa`
   explicit conversions. Pilot consumer: **`akuma-pmm`** (52 sites, and its
   bitmap is a `Vec<u64>` of PFNs — the densest unit-confusion surface per
   line of code in the tree). Effort: ~1 day for types + pilot, mirroring the
   `MmioReg` sweep's shape. **`akuma-mmu` (112 sites) is its own later sweep**
   with the full verify-trim gate (Tier 4 required), exactly as PTE Stage B is
   staged — one axis per sweep, per the TRIM_FAT rule.
2. **Syscall boundary: reshape, don't type.** Fix the 5 glue fns with
   slices/struct-params; a `Len` newtype constructed and unwrapped in the same
   function is the net-zero case the MMIO outcome documented.
3. **Gate, budget-style rather than grep:** add a fourth measure
   `addr_params` to `scripts/cloc_akuma.py` + `scripts/check_budgets.py`
   (count address-named usize fn params per component), seed with `--update`.
   Any new untyped-address signature then fails review until the budget diff
   carries it — the same ratchet that froze `unsafe`/`copy`/`mut`, and it
   covers exactly what greps can't see (new signatures in unlisted files).

## Checked and ruled out

- `MmioReg`, `Pte`/`TableRef` — decided in their own docs; not re-litigated.
- Identity phys↔virt makes a *conversion* newtype pointless today — but not
  the *distinctness* type; scope accordingly (this audit's axis is
  distinctness, per finding A).
- `fd`/`pid`/slot-index newtypes: tables are already fixed-size arrays with
  generation guards (see VEC_AUDIT's ruled-out list); no audit evidence of
  confusion bugs; defer until one appears.

## Suggested next step

Land the `addr_params` measure (small diff to two scripts), run `--update`,
and review the 248-site seed diff — its hot files are the honest priority
order for conversion sweeps. Then decide the `akuma-pmm` pilot by that diff.

## Background

- [`TRIM_FAT_MMIO_NEWTYPE.md`](TRIM_FAT_MMIO_NEWTYPE.md) /
  [`TRIM_FAT_PTE_NEWTYPE.md`](TRIM_FAT_PTE_NEWTYPE.md) — the doctrine and the
  pending Stage B.
- [`VEC_AUDIT.md`](VEC_AUDIT.md) — context-over-shape lesson, third citation.
- [`CLONE_AND_MUT_BUDGETS.md`](CLONE_AND_MUT_BUDGETS.md) — the budget
  mechanism the `addr_params` measure would extend.
