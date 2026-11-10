# Typed addresses and ownership types

**Grade: B** — the types and the gate are landed; the conversion of the callers is in progress (status table below). The rules
here are stable; the numbers move.

Two ideas, one discipline. **A raw `usize`/`u64` should not carry a meaning that only a variable name states** (addresses), and **an
`unsafe` block should not re-assert a claim that a value's constructor can make once** (ownership). Both are about moving a proof
from the use site to a type.

## Address types (`akuma_primitives::addr`)

| type | inner | meaning |
|---|---|---|
| `PhysAddr` | `u64` | a physical address (52 bits architecturally, so not `usize`-shaped by definition) |
| `VirtAddr` | `usize` | a virtual address; becomes a pointer, so it is `usize` |
| `Pfn` | `u64` | a page frame number: `PhysAddr >> PAGE_SHIFT` |

All three are `#[repr(transparent)]`, `Copy`, ordered and hashable. `PAGE_SIZE` (4096) and `PAGE_SHIFT` (12) live beside them.

**The operator set is the point and is deliberately small:**

* `addr + n`, `addr - n` (n a byte count) give an address; `addr - addr` gives a byte count, never an address;
* `Pfn + n` is the frame n on; `Pfn - Pfn` is a frame count;
* **no** `addr + addr`, no `addr * n`, no arithmetic across kinds, no `From<usize>`: a `VirtAddr` cannot reach a function that wants a `PhysAddr`;
* `Pfn <-> PhysAddr` is explicit (`PhysAddr::pfn()` rounds down; `Pfn::phys()` is `None` on overflow);
* page helpers: `is_page_aligned`, `page_offset`, `align_down`, `align_up` (`None` on overflow), `checked_add`, `checked_sub`.

These types buy **distinctness, not conversion.** Translation is the identity on AArch64 and a constant offset on amd64
(`addr.rs`: `PHYSMAP_OFFSET`, `phys_to_virt`), which is exactly why VA/PA confusion is cheap to write and invisible to a host test. Do not add
a method that hides the translation.

**Construction is explicit and rarer than use.** `PhysAddr::new(u64)` / `PhysAddr::from_usize(usize)` is where a raw number enters; code that
receives an address from the PMM or a page-table entry should hold it typed from there. If you find yourself wrapping and unwrapping in one
function, the type has no job there (the "net-zero conversion" case in the MMIO and PTE newtype outcomes).

### Who uses them

| crate | state |
|---|---|
| `akuma-primitives` | defines them |
| `akuma-phys` | **typed**: `phys_ptr(PhysAddr)`, `Frame::from_pa(PhysAddr)`, `Frame::pa()` and `FreshFrame::pa()` return `PhysAddr`, `copy_from_frame(PhysAddr)` |
| `akuma-pmm` | next: its API (alloc/free/CoW refcount/`init`/`add_ram`/`reserve_range`) is `usize` with 53 untyped parameters and ~880 references outside the crate; convert behind a typed twin per function |
| `akuma-pagetable`, `akuma-pte-*`, `akuma-mmap` | then; `akuma_mmap::PhysFrame` stays the `Copy` address vocabulary for crates that cannot call the PMM |
| `akuma-frame-raw`, `akuma-mmu-raw`, `akuma-switch`, `akuma-entry` | the raw leaves the `unsafe` sweep isolated: each takes a raw address behind one stated contract, so their untyped parameters (25) arrived with the `unsafe` and are budgeted, not converted. Type them with the wave that types their callers (`akuma-pmm`/`akuma-pagetable` below them, `akuma-threading` above) |
| `akuma-machine-facts` | budgeted (3): `Range`/`contains`/`ram_containing` are the discovery vocabulary for spans read out of VMM tables; typing it is a fold into `PhysAddr` at the kernel-core merge, with its adapters |
| `akuma-mmu` (130 params) | its own sweep, full verify-trim gate |
| `akuma-exec`, `akuma-exceptions`, `akuma-tokio-kernel-*`, `amd64/src/mm.rs` | consumers, last |

Order is bottom-up so each wave's callers can pass typed values from the wave below. Not every hit of the gate below is an address
confusion (a user `addr` in a socket call, a device bus `base`): those want a different type or none, and are triaged out of the list as the sweeps reach them.

## The gate: `scripts/check_addr_params.py`

Same shape as `scripts/check_unsafe_budget.py`. A component's count of **untyped address parameters** may not exceed
`docs/reference/addr-params-budget.json` (a component without an entry has budget 0). The way through is deliberate: `--update`, and the budget file's diff
carries the increase into review.

What counts: a parameter of a production `fn` whose name is an address word (`va`, `vaddr`, `pa`, `paddr`, `pfn`, `phys`, `addr`, `base`, also with
a prefix or suffix) and whose type is exactly `usize` or `u64`. Test files, `tests/` directories and inline `#[cfg(test)] mod name { .. }` blocks are
excluded (brace-matched, not truncated: `akuma-pmm` interleaves five of them with production code). It is a scan, not a parser.

```
python3 scripts/check_addr_params.py             # check (exit 1 on a violation)
python3 scripts/check_addr_params.py --update    # rewrite the budget; a decrease ratchets down
python3 scripts/check_addr_params.py --list crates/akuma-pmm   # the sites, for a conversion sweep
python3 scripts/check_addr_params.py --self-test
```

**Do not claim an `unsafe` win for a pure distinctness change.** Converting a signature to `PhysAddr` removes no `unsafe` site; the
reduction comes from the ownership types it enables.

## Ownership types (the `unsafe` side)

The `unsafe` left in the kernels after the instruction wrappers (`akuma-cpu`) is mostly a claim about *who owns something right now*. The pattern that
retires it, with the instances that exist:

> A value whose **constructor makes the claim once** (and checks it where it can be), and whose **methods are safe**.

| type | claim made at construction | where |
|---|---|---|
| `Frame` | `from_pa`: this is plain RAM no Rust object lives in (alignment and physmap containment checked) | `akuma-phys` |
| `FreshFrame` | allocated, exclusive, not yet mapped; `publish` consumes; drop frees | `akuma-phys` |
| `StackRegion` | `[base, base+size)` is mapped writable stack memory; every access range-checked | `akuma-stack` |
| `IrqFrame` | `at(sp)`: `sp` is a saved or live IRQ trap frame; typed `elr`/`spsr`, one definition of the layout | `akuma-trap-aarch64` |
| `FrameHandle` | the frame the vector just pushed for this handler | `akuma-trap-aarch64` |
| `DmaArray` | a `static` DMA buffer with a borrow word that turns an overlapping `&mut` into a panic | `akuma-dma` |
| `peek(idx, CtxField)` | display-only read of a thread slot you do not own: one relaxed word load, not for decisions | `akuma-threading` |

Rules (these are the review checklist):

* The constructor carries the `# Safety` section and the argument; if a method cannot be safe, split the type instead of adding `unsafe` methods.
* Per-core capabilities are `!Send`/`!Sync`; per-slot capabilities are consumed by an explicit `publish`/`abandon` — **no `Drop` that repeats a protocol step** (only `FreshFrame`, whose drop is the PMM free, does that).
* Never widen `akuma_mmap::PhysFrame` — it is a `Copy` address, and giving it powers would let any copy claim ownership.
* Do not use `FrameHandle` for another thread's frame; use `IrqFrame` (its contract allows a switched-out thread's saved frame).
* Missing types and their designed shapes (`PinnedFrame<'t>`, `SlotClaim`, `LocalCpu`, `SvmEnabled`) are specified in the archive doc below; none is landed.

## Background
[`../archive/NEWTYPE_AUDIT.md`](../archive/NEWTYPE_AUDIT.md) (the survey, bug ranking and the wave status table),
[`../archive/UNSAFE_OWNERSHIP_TYPES_DESIGN.md`](../archive/UNSAFE_OWNERSHIP_TYPES_DESIGN.md) (the ownership types still to build, in order),
[`../archive/UNSAFE_CONTEXT_TABLE_DESIGN.md`](../archive/UNSAFE_CONTEXT_TABLE_DESIGN.md) (the thread-context table and why part of it was re-scoped),
[`../archive/UNSAFE_KERNEL_CPU_STEP.md`](../archive/UNSAFE_KERNEL_CPU_STEP.md) (what moved and the metal verification),
[`crate-safety.md`](crate-safety.md) (which crates forbid `unsafe`), `TRIM_FAT_MMIO_NEWTYPE.md` / `TRIM_FAT_PTE_NEWTYPE.md` (the newtype doctrine: pay only when construction is rarer than use).
