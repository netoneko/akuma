# Unsafe audit, remaining sites: the ownership types they are waiting for

**Date:** 2026-11-06. **Status:** design, nothing landed. **Reads with:** [`UNSAFE_KERNEL_AUDIT.md`](UNSAFE_KERNEL_AUDIT.md) (B1, B2, B6, B7),
[`UNSAFE_CONTEXT_TABLE_DESIGN.md`](UNSAFE_CONTEXT_TABLE_DESIGN.md) (its 2026-11-06 update says which items were re-scoped *because the type was missing*),
`NEWTYPE_AUDIT.md` (2026-11-05, in the working tree, not yet committed - the address-vocabulary survey this builds on),
[`UNSAFE_KERNEL_CPU_STEP.md`](UNSAFE_KERNEL_CPU_STEP.md) (what is already done). **Audience:** the implementer.

## 0. The argument in one paragraph

After parts 1-3 the `unsafe` that is left in the kernels is almost never an *instruction* (those are in `akuma-cpu`) and almost never an *address calculation*
(those are in `akuma-phys`/`akuma-pagetable`). It is a claim about **who owns something at this moment**: this frame is mine alone, this slot is mine until I publish
it, this VMCB is not being run, this is the CPU whose GDT I am editing. Every re-scoped item in the context-table design stopped at the same wall: the
claim is true at the site but nothing in the *types* says so, so each use re-asserts it with an `unsafe` block. The fix is the same each time and is the
pattern `akuma_phys::FreshFrame` already proves: **a value whose constructor is the one place the claim is made (and checked where it can be), and whose
methods are safe.** The count drops because the claim is made once, at the constructor, instead of at every use; and the constructor is where a reviewer looks.

This document specifies the types, in dependency order, with the sites each one retires, so that each can be a separate, individually-verifiable change.

## 1. Foundation: the address vocabulary (`NEWTYPE_AUDIT.md`, recommendation 1)

`akuma-primitives` (dependency-free leaf) gains `PhysAddr`, `VirtAddr`, `Pfn` as that audit specifies: `#[repr(transparent)]`, operator-minimal (`PA + delta`, `PA - PA -> delta`,
no `PA + PA`), explicit `Pfn <-> PhysAddr`. That audit's axis is **distinctness**, and it lists 248 raw-`usize` address parameters with `akuma-pmm` (52) as the pilot.

What this document needs from it is narrower, and should not wait for the `akuma-mmu` sweep:

* `akuma_phys::phys_ptr<T>(pa: u64)` and `Frame::from_pa(pa: u64)` take `PhysAddr`; `FreshFrame::pa()` returns `PhysAddr`. A `VirtAddr` can no longer be handed to the physmap by mistake - the
  VA/PA confusion class that heads the audit's bug ranking, and the one that makes every `phys_ptr` call site a place a reviewer must reason about the argument's *kind*.
* `PhysAddr::page(self) -> Option<PageAddr>` (aligned) and `Frame::from_pa(PageAddr)`: the alignment `assert!` in `from_pa` becomes a type, and the physmap-containment check
  stays a runtime check at `from_pa` (it depends on the machine's RAM map, which a type cannot know).

**Do not** introduce `PhysAddr` arithmetic that hides the identity-map fact. The audit is right that identity translation makes a *conversion* type pointless today.

Gate: the audit's own proposal - an `addr_params` measure in `scripts/cloc_akuma.py` / `check_budgets.py` - lands **first**, so the type sweep has a ratchet and an honest hot-file order.

## 2. Frame ownership: three states, three types

`akuma-phys` already has two of the three; the exceptions and shmpages sites are stuck on the missing one.

| state | meaning | type | exists? |
|---|---|---|---|
| **plain RAM** | some frame; no claim about who else touches it; reads/writes are volatile word/byte ops | `Frame` (`from_pa` unsafe once) | yes |
| **staged** | allocated, exclusive, not yet mapped/tracked; `&mut [u8; 4096]` is sound | `FreshFrame` (`publish` consumes) | yes |
| **pinned** | live, shared, kept alive by a table entry for the length of a closure; other mappers may read it | `PinnedFrame<'t>` | **no** |

### 2.1 Use `FreshFrame` where the code already is "staged"
`akuma-exceptions` demand-fill (two sites, file read straight into a fresh page) and the CoW-break / trampoline-install copies are *staged* frames that happen to be carried as
`akuma_exec::PhysFrame` - a `Copy` address with no ownership (`akuma-mmap::PhysFrame`; it must stay that, `MmapRegion` holds `Vec<PhysFrame>` for crates that cannot call the PMM).
So the fix is not to give `PhysFrame` powers; it is to keep the fill stage in `FreshFrame` and convert at the edge:

```rust
impl FreshFrame {
    pub fn alloc_batch_zeroed(count: usize) -> Option<Vec<FreshFrame>>;      // replaces alloc_pages_zeroed + wrapping in PhysFrame
    pub fn fill_with<R>(&mut self, off: usize, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R;   // safe: bounds-checked slice of bytes_mut()
    pub fn publish(self) -> PhysFrame;                                       // (today: -> usize) hand to the mapper as the address vocabulary type
}
```
`demand_fill` becomes `frame.fill_with(dst_off, len, |buf| read_at(path, off, buf))` - the file read lands in the frame with no `from_raw_parts_mut`. The drop-frees-the-frame behaviour
is the real win: the current `frame_pool: Vec<PhysFrame>` leaks every unconsumed frame on an early return, and the existing comment on `FILL-SHORT` already notes the frame is in an
indeterminate state. Retires: the two demand-fill slices, the CoW `copy_nonoverlapping` (`FreshFrame::copy_from_frame` exists), the trampoline copy (`fill_with`). **~5 sites in `akuma-exceptions`.**
Check first: the `frame_pool` batch is consumed across a loop with early exits; model the pool as `Vec<Option<FreshFrame>>` and `publish` each as it is mapped, so a mid-loop failure drops (frees) the rest.

### 2.2 `PinnedFrame<'t>` for table-held shared frames
`crates/akuma-kernel-amd64/src/shmpages.rs` (`wrote`, `zeroed`) writes through the physmap to a frame that the shared-page table pins: "the table holds a reference for as long as the entry exists, and the entry cannot go
while this hold is taken". That sentence *is* the type:

```rust
pub struct PinnedFrame<'t> { frame: Frame, _hold: PhantomData<&'t ()> }     // constructed only by the table's iterator, which holds the lock
impl PinnedFrame<'_> {
    pub fn write_bytes(&self, off: usize, src: &[u8]);        // overlap-safe (copy, not copy_nonoverlapping), bounds-checked
    pub fn zero(&self, off: usize, len: usize);
    pub fn pa(&self) -> PhysAddr;
}
```
`for_each_in` hands `PinnedFrame<'_>` to the closure instead of a raw `pa`; the lifetime ties the handle to the closure, so it cannot be stashed past the hold. The constructor is `unsafe` once, inside the table
module (`from_pa` + the pin argument). Retires the two shmpages sites into one, and gives `akuma-fpcache-rw` the same handle if it has the same shape (**check it before promising**: if its table is generic over the
payload, the handle goes in the table crate, not in `amd64/`). `Frame::write_bytes` today is `copy_nonoverlapping` (documented "as `read_bytes`"); `PinnedFrame::write_bytes` must be the overlap-safe variant because
its source can be a physmap slice of another pinned frame - the case the existing comment says "nothing here proves".

## 3. Slot ownership: `SlotClaim` and what the kernels' fixed slots really are

`UNSAFE_CONTEXT_TABLE_DESIGN.md` §3.2 specified `Initializing` and then re-scoped it, because `x86_seed_entry(slot, ..)` is public and three kernels call it with slot ids that did **not** come from
`claim_free_slot` (`IDLE_TID`, fixed task slots). Two cases have to be told apart before any type is chosen, and that is the first task here:

1. Are those fixed slots ever `FREE` when seeded? (Memory note: the amd64 kernels never call `akuma_exec::init`; the slot recycler is not registered. If the slots are simply never claimed, then the
   claim argument for them is *"nobody else can pick this slot, because the scheduler never sees it as FREE"* - a different, weaker argument than the CAS.)
2. If they are claimed by `compare_exchange` somewhere, the token can be produced there and the public API takes it by value.

Proposed type either way, public, in `akuma-threading`:

```rust
#[must_use] pub struct SlotClaim { idx: usize }            // !Copy, !Clone
impl SlotClaim {
    pub fn claim_free(range: Range<usize>) -> Option<Self>;           // today's claim_free_slot (FREE -> INITIALIZING, SeqCst CAS)
    pub fn claim_fixed(idx: usize) -> Result<Self, SlotBusy>;          // case 1/2 above: CAS FREE -> INITIALIZING for a slot a kernel owns by name
    pub fn idx(&self) -> usize;
    pub fn ctx(&mut self) -> &mut Context;                             // safe; the unsafe is the CAS argument, written once
    pub fn publish(self);                                              // = mark_thread_ready; consumes
    pub fn abandon(self);                                              // INITIALIZING -> FREE on the error paths
}
pub fn x86_seed_entry(slot: &mut SlotClaim, stack_top: usize, entry: extern "C" fn() -> !);
```
`#[must_use]` plus explicit `publish`/`abandon` (no `Drop`: several paths hand the slot to another core, a silent Drop would double-free). Retires: the 8 internal writer sites (the spawn paths and
`adopt_current_as_core_idle`) *and* makes `x86_seed_entry`'s `get_context_mut` safe. It changes the signature in four crates, so it is its own change **with an amd64 metal boot in the same session** (the aarch64 half is
bootable under HVF locally). `claim_fixed` is the open question; if (1) shows the fixed slots are never `FREE`, add a `FixedSlot` constructor whose doc states the weaker argument and `debug_assert`s the state is not `READY`.

`update_thread_context` (execve/fork, a *live* slot) is not a claim; its own narrow fn (`set_user_entry` on a child's claim; `with_current_ctx_mut` for self) is in the context-table doc §3.2. `ThreadId`/`SlotId` newtypes
(NEWTYPE_AUDIT "ruled out": no confusion bugs on record) stay out of scope; `SlotClaim` is a capability, not an id type.

## 4. Per-CPU state: `LocalCpu`

`crates/akuma-kernel-amd64/src/gdt.rs` (3 sites), `akuma-tokio-kernel-amd64` (`set_rsp0`, GDT/TSS init, 3-5 sites) and the `GS_BASE` write all mean the same claim: *I am this CPU, no other core touches its slot of this table, and I cannot be preempted off it.*

```rust
pub struct LocalCpu { id: CpuId, _not_send: PhantomData<*mut ()> }      // !Send !Sync: cannot leave the core it was made on
impl LocalCpu {
    pub unsafe fn assume() -> Self;      // THE site: called once per core from boot/AP-entry with interrupts masked; reads the LAPIC id / GS slot and records it
    pub fn id(&self) -> CpuId;
    pub fn gdt(&mut self) -> &mut PerCpuGdt;     // &mut self: one live borrow per core, by construction
    pub fn set_rsp0(&mut self, top: u64);
}
```
`akuma-gdt` already owns the descriptor encoding and gains `PerCpuGdt` owning the statics (audit B7). `LocalCpu` lives in `akuma-cpu` or a tiny `akuma-percpu` crate so both amd64 kernels use one. The `GS_BASE` write
**stays `unsafe`** (it re-points state every per-CPU read dereferences; `UNSAFE_KERNEL_CPU_STEP.md`), but it moves into `LocalCpu::assume`'s one site. Retires ~8 sites. Verified only by an SMP boot: TCG at `SMP=4` plus metal.

## 5. The SVM run loop: a token that proves SVM is on

`crates/akuma-kernel-amd64/src/kvm.rs` + `kvm/linux.rs`: the `EFER.SVME`/`VM_HSAVE_PA` enable pair, five `vmrun` call sites, `vmrun`/`vmrun_regs`/`vmsave` and their asm (audit B1: ~14 sites). The claims are (a) SVM is enabled on *this* core before any
`vmrun`, (b) the VMCB being run is a page nothing else touches, (c) the host-save area is registered.

```rust
pub struct SvmEnabled { _not_send: PhantomData<*mut ()> }                 // ZST, !Send: proof for THIS core
impl SvmEnabled { pub unsafe fn enable(hsave: &HostSaveArea) -> Result<Self, SvmUnavailable>; }   // the one unsafe: EFER write + HSAVE MSR
pub struct VmcbPage(FreshFrame)                                         // owned, exclusive; as_vmcb_mut() -> &mut Vmcb (akuma-svm-amd64's layout types)
impl SvmEnabled { pub fn run(&self, vmcb: &mut VmcbPage, regs: &mut GuestRegs) -> ExitInfo; }     // safe: the asm is inside, the preconditions are the arguments' types
```
New leaf crate `akuma-svm-run-amd64` (keeps `akuma-svm-amd64` at `forbid(unsafe_code)` with no dependencies, per its CLAUDE.md entry); it owns the `global_asm!`. `GuestRegs` is the 16-GPR block the asm
saves/restores, `#[repr(C)]` with a layout test against the asm's offsets (the same style as the `UserTrapFrame` layout assertions). **Verification caveat that decides sequencing: SVM exists only on the ryzen (metal) and
under nested virt; TCG has no SVM.** This is the one item here with no Mac test - schedule it for a session with the box in hand, behind the existing `kvmtest` GRUB entry, and keep the old code until the new path passes the `kvm-linux` + Akuma-guest phases.

## 6. Smaller items that fit existing crates (no new type)

* `uaccess::read_val`/`write_val` (`MaybeUninit` pair) move into `akuma-user-access`, which owns `copy_from_user_safe` and the fault-fixup contract (audit C).
* `signal.rs::deliver_pending`: not examined this round - read it before assigning a type; it writes the user signal frame, so the likely shape is a `UserStackWriter` over `copy_to_user_safe` (no new `unsafe`).
* The 4-level walk duplicated in `akuma-tokio-kernel-amd64` vs `akuma_pte_amd64` (audit C): delete the copy.

## 7. Order, sizes, and what each costs to verify

| # | change | sites retired | verifiable on the Mac? | notes |
|---|---|---|---|---|
| 1 | `addr_params` measure + `PhysAddr`/`VirtAddr`/`Pfn` in primitives, `akuma-phys` signatures only | 0 (distinctness) | yes, host tests | NEWTYPE_AUDIT's order; do not start `akuma-mmu` |
| 2 | `FreshFrame::{alloc_batch_zeroed, fill_with}`; exceptions demand-fill/CoW/trampoline | ~5 | aarch64 under HVF (`mw_gate`) | also fixes the frame_pool leak-on-early-return |
| 3 | `PinnedFrame<'t>` for shmpages (+ fpcache-rw if same shape) | ~1-2 | amd64 TCG | check the table's genericity first |
| 4 | `SlotClaim` and `x86_seed_entry(&mut SlotClaim)` | ~9 | aarch64 yes; amd64 TCG+metal | answer §3 questions 1/2 before coding; own PR |
| 5 | `LocalCpu`, `PerCpuGdt` | ~8 | TCG `SMP=4`; metal | `GS_BASE` stays unsafe |
| 6 | `SvmEnabled` + `akuma-svm-run-amd64` | ~14 | **metal only** | last; needs the box |
| 7 | `read_val`/`write_val` to `akuma-user-access`; delete the duplicated walker | ~4 | host + TCG | cheap, can go any time |

Expected total: ~40 of 472, which is the honest ceiling for the kernels; the rest is `akuma-cpu` and the leaf crates, where it belongs. Every change keeps a `# Safety` section on the constructor,
a `SAFETY:` comment stating the *argument*, and moves `scripts/check_unsafe_budget.py --update` in the same commit.

## 8. Rules that apply to all of them

* The constructor makes the claim; methods are safe. If a method cannot be safe, the type is wrong - split it, do not add `unsafe` methods.
* Capabilities are `!Send`/`!Sync` when they are per-core (`SvmEnabled`, `LocalCpu`) and consumed by `publish`/`abandon` when they are per-slot (`SlotClaim`); never `Clone`/`Copy`.
* No `Drop` that does something the surrounding protocol also does (slots), except `FreshFrame`, whose drop is the PMM free and is already the established contract.
* Never widen `akuma_mmap::PhysFrame` (a `Copy` address used by crates that cannot call the PMM).
* No allocation on the fault path: `fill_with` hands out a slice of the frame; do not buffer.
* Do not change `Context`'s layout, the `SeqCst` publication, or `MAX_THREADS`.

## Background
`akuma_phys::FreshFrame` (the pattern), `akuma_trap_aarch64::irq_frame::IrqFrame` (the same shape, landed), `akuma_dma::DmaArray` (a borrow word as a runtime ownership proof), `akuma-fdtable` (state machine, no locks).
