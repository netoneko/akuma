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

## Implementation status (tracked from 2026-11-06)

Order follows the audit's own recommendation, with one deliberate change: the first converted crate is `akuma-phys`, not `akuma-pmm`. The audit chose
`akuma-pmm` as the densest unit-confusion surface; `akuma-phys` is the **door** — every `Frame::from_pa`, `phys_ptr` and `FreshFrame::pa` is where a raw number
becomes a pointer — and its `unsafe` is what `UNSAFE_OWNERSHIP_TYPES_DESIGN.md` (§1) needs typed first. Bottom-up, so each wave's callers can pass typed values
from the wave below: **primitives -> phys -> pmm -> mmu/mmap/pagetable -> exec/exceptions/syscalls-glue -> amd64**.

| wave | crate | what | state |
|---|---|---|---|
| 0 | gate | `scripts/check_addr_params.py` + `docs/reference/addr-params-budget.json` (the `addr_params` ratchet; `--list [COMPONENT]` prints the sites). Seeded at **615** | done |
| 1 | `akuma-primitives` | `PhysAddr`, `VirtAddr`, `Pfn`, `PAGE_SIZE`/`PAGE_SHIFT` in `addr_types.rs`, re-exported from `addr`; 6 host tests; operator-minimal as specified | done |
| 2 | `akuma-phys` | `phys_ptr(PhysAddr)`, `Frame::from_pa(PhysAddr)`, `Frame::pa()`/`FreshFrame::pa() -> PhysAddr`, `copy_from_frame(PhysAddr)`; callers in `amd64`, `akuma-tokio-kernel-amd64`, `akuma-exceptions-forensics` adapted (`publish() -> usize` unchanged until `akuma-pmm`) | done (612) |
| 3 | `akuma-pmm` | `alloc_page*`/`free_page*`/`cow_ref_*`/`is_page_free`/`contains`/`init`/`add_ram`/`reserve_range` take and return `PhysAddr`/`Pfn`; **883 references outside the crate** (`grep -rn 'akuma_pmm::\|pmm::'`), so convert behind a typed twin per function and move callers wave by wave, deleting the `usize` twin at the end | next |
| 4 | `akuma-pagetable`, `akuma-pte-*`, `akuma-mmap` | table-frame and PTE address params; `akuma_mmap::PhysFrame` stays the `Copy` address vocabulary but wraps a `PhysAddr` | |
| 5 | `akuma-mmu` | 130 params in one file; its own sweep with the full verify-trim gate (Tier 4), per the audit | |
| 6 | `akuma-exec`, `akuma-exceptions`, `akuma-tokio-kernel-{core,aarch64,amd64}`, `amd64/src/mm.rs` | consumers | |

**Reading the hot list.** `check_addr_params.py` is wider than the audit's 248 (it scans the whole tree and the words `addr`/`base`), so the top of
`--list` needs triage: `akuma-syscalls-glue/{net,mem}.rs` (user-space `addr` for a socket address or an `mmap` hint) and `akuma-xhci/src/trb.rs` (device-bus addresses)
are not VA/PA confusion and should move to a different type (`UserAddr`) or stay. The mm-domain conversion order is `akuma-mmu` (130) -> `amd64/src/mm.rs` (24) ->
`akuma-exec` (`children.rs` 20) -> `akuma-tokio-kernel-*` -> `akuma-exceptions` (16).

**What the types did and did not do for `unsafe`.** Nothing by themselves: wave 2 changed no `unsafe` site (472 before and after). The reduction comes from what the
types enable - `Frame::from_pa(PhysAddr)` is where the alignment assert became a type, and the ownership types in `UNSAFE_OWNERSHIP_TYPES_DESIGN.md` are built on
`PhysAddr`/`Pfn`. Do not claim an `unsafe` win for a pure distinctness wave.

Verification of waves 0-2: host tests (`akuma-primitives` 76, incl. the 6 new), clippy `-D warnings` on primitives/phys/forensics and both amd64 kernels, builds of
`akuma-amd64`, `akuma-tokio-kernel-amd64`, `shanghai`, `taoxichuan` (`x86_64-unknown-none`) and the aarch64 kernel; `mw_gate.sh all` at `SMP=4`.
