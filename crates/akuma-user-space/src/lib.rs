//! The **frame ledger** of a user address space: which physical frames it
//! holds, how many virtual addresses map each one, and who owes the PMM a free.
//!
//! No page tables. No `TTBR0`, no ASID, no TLB, no PTE encoding — nothing that
//! knows what architecture it is on. A [`FrameLedger`] is a `BTreeMap` and a
//! `Vec` behind two spinlocks, plus the one rule that connects them to the
//! kernel's global CoW refcount.
//!
//! # Why this is a crate
//!
//! `akuma_mmu::UserAddressSpace` is `#[cfg(target_arch = "aarch64")]` and 45
//! methods across ~457 lines. Splitting it by what the code actually *does*:
//!
//! | | methods | lines |
//! |---|---|---|
//! | page-table walk + ISA | 32 | 386 |
//! | this ledger | 13 | 71 |
//!
//! The walker is genuinely architecture-specific and stays. The ledger never
//! was: it was only aarch64-gated because it shared a struct with the walker.
//! Three of `UserAddressSpace`'s five fields live here now, and the amd64
//! target can hold one without acquiring an opinion about `TTBR0`.
//!
//! This does **not** by itself make `akuma-exec` build for x86_64 — that crate
//! calls the walker too. What it does is make the walker's remaining content
//! honest, and put a host test around the part of an address space that has
//! historically been wrong.
//!
//! # Two counts, and the rule between them
//!
//! There are two reference counts over the same physical frame and they count
//! different things:
//!
//! * **`user_frames` (here)** counts *virtual addresses per physical frame,
//!   within one address space*. A frame mapped at two VAs in one process has a
//!   count of 2.
//! * **`akuma_pmm::COW_REFCOUNTS` (global)** counts *address spaces*. That same
//!   frame contributes exactly **1**, however many VAs map it.
//!
//! The rule joining them — "an address space contributes exactly one global
//! reference however many VAs it maps" — was maintained by hand at roughly
//! forty call sites, and splitting the two updates across a lock hold is what
//! once let the global count drift below the truth and hand a live frame back
//! to the PMM (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6).
//! [`FrameLedger::adopt_user_frame`] is where the two are updated as one
//! uninterruptible unit, and it is the reason this crate depends on
//! `akuma-pmm`: the rule cannot be enforced from outside the lock that owns the
//! per-address-space half of it.
//!
//! # Teardown frees each frame once
//!
//! [`FrameLedger::remove_user_frame`] returns whether the caller now owns the
//! free — true only on the transition to zero. Teardown drains the map with
//! [`FrameLedger::take_user_frames`] and frees each **distinct** key once,
//! regardless of its count. Both are the existing behaviour; what is new is
//! that they are testable without a kernel.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use akuma_mmap::PhysFrame;
use akuma_primitives::irq::IrqGuard;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use spinning_top::Spinlock;

pub mod instr;

/// Every physical frame one user address space is responsible for.
///
/// Both maps are individually locked rather than sharing one lock: they are
/// touched on different paths (`user_frames` on every fault, `page_table_frames`
/// only when the walker allocates a table) and nothing needs a consistent view
/// of the pair except teardown, which takes them one after the other and is the
/// only writer left by then.
///
/// Every lock is taken under an [`IrqGuard`]. That is not defensive: these locks
/// are acquired from fault context, and a timer landing mid-hold would schedule
/// a thread that takes the same lock and spin forever — the same rule the PMM's
/// own tables follow.
///
/// The guard must outlive the lock, or IRQs come back while the spinlock is
/// still held and the deadlock is back. Every method here writes the acquire as
/// a tail expression, which under **edition 2024** drops the temporary lock
/// guard *before* the block's locals — `_irq` last, which is the order wanted.
/// Under the older tail-expression scope the temporary outlived the locals and
/// the order was inverted, so this is one place where the edition is
/// load-bearing rather than cosmetic.
pub struct FrameLedger {
    page_table_frames: Spinlock<Vec<PhysFrame>>,
    /// Physical address -> how many VAs in *this* address space map it.
    ///
    /// A map rather than a `Vec` so [`Self::remove_user_frame`] is O(log n)
    /// instead of a linear scan: `munmap`/exit tears down `P` pages in
    /// O(P·log n) rather than O(P·n).
    user_frames: Spinlock<BTreeMap<usize, u32>>,
    /// A borrowed view of another address space's tables (a `vfork` child).
    /// A shared view owns no frames and must free none — the owner does.
    shared: bool,
}

impl FrameLedger {
    /// An empty ledger. `shared` marks a borrowed view, which owns nothing.
    #[must_use]
    pub const fn new(shared: bool) -> Self {
        Self {
            page_table_frames: Spinlock::new(Vec::new()),
            user_frames: Spinlock::new(BTreeMap::new()),
            shared,
        }
    }

    /// Is this a borrowed view of someone else's tables?
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        self.shared
    }

    /// Record one more VA mapping `frame` in this address space.
    #[allow(clippy::significant_drop_tightening)]
    pub fn track_user_frame(&self, frame: PhysFrame) {
        let _irq = IrqGuard::new();
        *self
            .user_frames
            .lock()
            .entry(frame.addr)
            .or_insert_with(|| {
                instr::uf_insert();
                0
            }) += 1;
    }

    /// Adopt `frame`, maintaining **both** this ledger and the global share
    /// count as one uninterruptible unit. See the module header for why that
    /// matters.
    ///
    /// `caller_holds_ref` says whether the caller already took a global
    /// reference for this adoption (`file_page_cache::lookup_and_ref` does, to
    /// keep the frame alive across the fill). Returns `true` when that
    /// reference was **surplus** — this address space already had the PA — and
    /// the caller must release it. Making that a return value rather than a
    /// separate `drop_surplus_shared_ref` call is what stops it being forgotten
    /// on one arm and not the other.
    ///
    /// Lock order: `COW_REFCOUNTS` is a leaf and is taken innermost, the same
    /// direction as the existing `as_lock` -> `COW_REFCOUNTS` order.
    pub fn adopt_user_frame(&self, frame: PhysFrame, caller_holds_ref: bool) -> bool {
        let _irq = IrqGuard::new();
        let mut uf = self.user_frames.lock();
        let first_va_here = !uf.contains_key(&frame.addr);
        if first_va_here {
            instr::uf_insert();
        }
        *uf.entry(frame.addr).or_insert(0) += 1;
        match (caller_holds_ref, first_va_here) {
            // The caller's reference becomes this address space's one reference.
            (true, true) => false,
            // Already had the PA: the caller's reference is surplus.
            (true, false) => true,
            // No reference taken, and this is the first VA — take one now.
            (false, true) => {
                akuma_pmm::cow_ref_inc(frame.addr);
                false
            }
            // Already counted by an earlier VA — nothing to add.
            (false, false) => false,
        }
    }

    /// Does this address space already hold `pa` as a user frame?
    ///
    /// Teardown frees each distinct PA exactly once however many VAs map it, so
    /// an address space contributes exactly one global reference per frame.
    /// Callers that take a reference per *fault* (the shared file-page cache)
    /// use this to spot the second VA onto an already-held frame and hand the
    /// surplus reference back, instead of leaking it until reboot.
    #[must_use]
    pub fn tracks_user_frame(&self, pa: usize) -> bool {
        let _irq = IrqGuard::new();
        self.user_frames.lock().contains_key(&pa)
    }

    /// Drop one VA's claim on `frame`. Returns `true` when the **last** one went
    /// away and the caller now owns the free.
    ///
    /// `false` for a frame this ledger does not track: not its obligation, and
    /// freeing it would be someone else's use-after-free.
    pub fn remove_user_frame(&self, frame: PhysFrame) -> bool {
        let _irq = IrqGuard::new();
        let mut frames = self.user_frames.lock();
        if let Some(count) = frames.get_mut(&frame.addr) {
            *count -= 1;
            if *count == 0 {
                frames.remove(&frame.addr);
                instr::uf_removed();
                return true; // last reference dropped — caller owns the free
            }
            return false; // still mapped at another VA — must not free yet
        }
        false // untracked here — not this address space's free obligation
    }

    /// Record an intermediate page-table frame the walker just allocated.
    pub fn track_page_table_frame(&self, frame: PhysFrame) {
        let _irq = IrqGuard::new();
        self.page_table_frames.lock().push(frame);
    }

    /// Distinct physical frames tracked as user data — one entry per PA,
    /// regardless of how many VAs map it.
    ///
    /// Leak debugging: compare against the VA actually mapped. A count far
    /// larger than the mapped VA means frames are being tracked but orphaned
    /// (a re-fault leak).
    #[must_use]
    pub fn user_frame_count(&self) -> usize {
        let _irq = IrqGuard::new();
        self.user_frames.lock().len()
    }

    /// Sum of every per-PA count — total VA->frame mappings tracked.
    #[must_use]
    pub fn user_frame_total_refs(&self) -> usize {
        let _irq = IrqGuard::new();
        self.user_frames.lock().values().map(|&c| c as usize).sum()
    }

    /// Page-table frames this address space holds.
    #[must_use]
    pub fn page_table_frame_count(&self) -> usize {
        let _irq = IrqGuard::new();
        self.page_table_frames.lock().len()
    }

    /// Frames this address space will hand back to the PMM when it drops:
    /// tracked user pages, intermediate page tables, and the top-level table.
    ///
    /// A **shared** view owns none of them (the owner does) and reports 0.
    ///
    /// The `+ 1` is that top-level table, which the ledger does not itself hold
    /// — it is the walker's `l0_frame`. Counted here because this is the number
    /// `process::reclaim` stamps at retirement to size the reclaimable backlog
    /// without dereferencing a RETIRED `Process`, and it wants the whole cost.
    #[must_use]
    pub fn resident_pages(&self) -> usize {
        if self.shared {
            return 0;
        }
        self.user_frame_count() + self.page_table_frame_count() + 1
    }

    /// Take the user-frame map for teardown, leaving the ledger empty.
    ///
    /// Teardown frees each **distinct key** once, ignoring the counts: the map
    /// counts VAs, and a frame mapped twice is still one frame to give back.
    #[must_use]
    pub fn take_user_frames(&self) -> BTreeMap<usize, u32> {
        let _irq = IrqGuard::new();
        core::mem::take(&mut *self.user_frames.lock())
    }

    /// Take the page-table frame list for teardown, leaving the ledger empty.
    #[must_use]
    pub fn take_page_table_frames(&self) -> Vec<PhysFrame> {
        let _irq = IrqGuard::new();
        core::mem::take(&mut *self.page_table_frames.lock())
    }
}

#[cfg(test)]
mod tests;
