# Extracting the frame ledger out of `UserAddressSpace`

**Date:** 2026-09-06
**Status:** shipped and A/B-verified. `crates/akuma-user-space`, 14 host tests.

## Why

`akuma_mmu::UserAddressSpace` is `#[cfg(target_arch = "aarch64")]`, and it is
what stops `akuma-exec` — and therefore `akuma-vfs-glue` and
`akuma-syscalls-glue` — from compiling for the amd64 target
(`docs/archive/AKUMA_ELF_ARCH_NEUTRAL.md`). The obvious move, a `UserPages`-style
trait like the one that freed `akuma-elf`, does not work here: `akuma-exec` calls
~25 of its methods, and a 25-method trait is a re-export with extra steps.

So the question was what the type is actually made of. Splitting the 45-method
impl by what the code does:

| | methods | lines |
|---|---|---|
| page-table walk + ISA (`TTBR0`, ASID, TLB, I-cache, PTE bits) | 32 | 386 |
| the frame ledger | 13 | 71 |

Three of its five fields — `page_table_frames`, `user_frames`, `shared` — are a
`Vec`, a `BTreeMap` and a `bool`. Nothing in them knows what architecture it is
on. They were AArch64-only because they shared a struct with the walker, and for
no other reason.

## What moved

`crates/akuma-user-space` — `no_std`, `#![forbid(unsafe_code)]`, dependencies
`akuma-mmap` (for `PhysFrame`), `akuma-primitives` (`IrqGuard`), `akuma-pmm`,
`spinning_top`. It holds `FrameLedger` and the `user_frames` leak counters.

`UserAddressSpace` went from five fields to three:

```rust
pub struct UserAddressSpace {
    l0_frame: PhysFrame,
    ledger: FrameLedger,   // <- was three fields
    asid: u16,
}
```

Its 10 ledger methods are now one-line delegations, and the five places that
reached past them into `self.page_table_frames.lock().push(..)` or
`core::mem::take(&mut *self.user_frames.lock())` go through
`track_page_table_frame` / `take_user_frames` instead. Nothing outside
`akuma-mmu` changed.

### The `akuma-pmm` dependency is deliberate

There are two reference counts over the same frame and they count different
things: `user_frames` counts *VAs per frame within one address space*;
`akuma_pmm::COW_REFCOUNTS` counts *address spaces*, to which a frame mapped at
five VAs still contributes exactly 1.

The rule joining them was maintained by hand at ~40 call sites, and splitting
the two updates across a lock hold is what once let the global count drift below
the truth and hand a live frame back to the PMM
(`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6). `adopt_user_frame` updates both
inside one `IrqGuard` + one `user_frames` hold. That rule **cannot** be enforced
from outside the lock owning the per-address-space half of it, so the crate
takes the PMM dependency rather than exposing a hook and hoping.

### The counters moved with the map they count

`akuma-mmu`'s `instr` module kept the address-space *lifecycle* counters
(`AS_NEW`, the drop bracket, the stuck-drop ring) and forwarded the six
`user_frames` ones. Leaving them split would have been worse than not moving
them at all: `track_user_frame` now increments the new crate's counter while
teardown incremented the old one, so `uf_flow_stats` would have reported a
residual that was pure bookkeeping artefact — on the one tool built to find real
leaks.

### One place where the edition is load-bearing

Every ledger method takes its lock as a **tail expression** under an `IrqGuard`
local. Under edition 2024 a tail expression's temporaries drop *before* the
block's locals, so the spinlock guard is released and only then are IRQs
restored. The other order re-enables interrupts with the spinlock still held,
which is the deadlock `IrqGuard` exists to prevent. Clippy's `let_and_return`
wants the tail form and is right — but for a reason that has nothing to do with
style, so it is written down at the struct.

## Host tests: 14

The point of the exercise. These pin behaviour that previously needed a booted
kernel and hours of self-host build to exercise:

- One frame at two VAs is **one entry with a count of two** — teardown frees per
  entry, and treating the count as the entry count double-frees.
- `remove_user_frame` transfers the free obligation only on the transition to
  zero; removing an untracked frame claims nothing (it is another address
  space's frame); removing past zero cannot resurrect an entry.
- `adopt_user_frame`'s four-way rule, including the case that matters: a second
  VA with a held reference reports it **surplus**. Answering `false` there is the
  leak-until-reboot bug; answering `true` on the first VA frees a live frame.
  Eight adoptions of one frame owe exactly one global reference and hand seven
  back.
- A **shared** view (a `vfork` child borrowing its parent's tables) reports
  `resident_pages() == 0`. Reclaim sizing a backlog from one would double-count
  the parent; a teardown acting on one would free pages still in use.

## Verified

Host suite and clippy clean on both targets; AArch64 booted at 165
`Result: PASS` / 97 `[PASS]`, one `[FAIL]` (the known clean-tree
`retired_reclaim_ab`).

The failure mode this refactor could introduce is a frame leak on process
teardown, which no host test can see. A/B against a `git worktree` of the
pre-change HEAD, same disk image, same 480 `/bin/true` spawns in four rounds:

| arm | free-memory delta over 480 spawns |
|---|---|
| baseline (HEAD, pre-ledger) | **-12608 KiB**, with one round at **+4576** |
| ledger | **-6104 KiB** |

Non-monotonic in both arms and the ledger arm is no worse, so the drift is
pre-existing churn, not a per-process leak introduced here. (Worktree trap worth
recording: a fresh `git worktree` has **no submodules**, and the build dies in
`akuma-fbcon/build.rs` on the missing Spleen font. Symlink
`crates/akuma-fbcon/vendor/spleen` from the main tree.)

## Where it gets us

Honestly: **not to a building `akuma-exec`.** That was never this step's job and
the numbers said so up front — the ledger is 16% of the type. The chain is
unchanged:

```
akuma-mmu, akuma-elf   OK for x86_64
akuma-exec             <- still the walker
akuma-vfs-glue         <- akuma-exec
```

What it does buy:

1. **The walker's remaining content is now honest.** `UserAddressSpace` is 32
   methods that all genuinely touch page tables, instead of 45 with a `BTreeMap`
   hidden among them. Any future judgement about trait-ifying it is made against
   the real number.
2. **The historically-buggiest accounting in the type is host-tested**, and can
   be reasoned about without an MMU.
3. **amd64 can hold one today.** The crate builds for `x86_64-unknown-none` now,
   so when that target grows CoW `fork` (blocker #1 in
   `AKUMA_AMD64_STREAMLINING.md` §11) it does not need to re-derive the
   two-counts rule — the bug that rule prevents has already been paid for once.

## What would come next, if anything

The remaining 32 methods are not one job either. In descending order of what
would actually shrink them:

- **Collapse the four `_no_flush` pairs.** They exist because AArch64 TLB
  maintenance broadcasts and is expensive; they duplicate that part of the
  surface for a policy decision that could be a parameter or a guard.
- **Audit `l0_phys()` — 44 uses, 30 bare, 6 cast straight to `*const u64`.**
  Callers are walking page tables outside the type that owns them. No trait can
  hold a line that is already being stepped over, so this decides whether the
  rest is possible at all.

Neither is required by anything today. This document exists so the next person
does not re-measure the split from scratch.

## Background

- `docs/archive/AKUMA_ELF_ARCH_NEUTRAL.md` — the measurement that named
  `akuma-exec` as the wall, and the `UserPages` trait that worked at three
  methods.
- `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6 — the drift that
  `adopt_user_frame` exists to prevent.
- `docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md` — the 1.47 M `user_frames` entries
  the leak counters were built for.
