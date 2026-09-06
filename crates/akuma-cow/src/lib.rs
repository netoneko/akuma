//! The copy-on-write write-fault **decision**, as a pure function.
//!
//! A write fault lands on a read-only user page. Four things can be true, they
//! need different responses, and telling them apart wrongly is silent in every
//! direction:
//!
//! | situation | wrong response | what you see |
//! |---|---|---|
//! | another thread already repaired the page | kill the process | a `SIGSEGV` on a write the page table *grants* |
//! | the page is read-only on purpose (`mprotect`) | copy it and grant the write | `mprotect(PROT_READ)` silently stops working |
//! | this is the only holder left | allocate and copy | correct, and needlessly slow on the commonest path |
//! | genuinely shared | grant the write in place | two processes silently share one page |
//!
//! Every one of those is a real incident in this tree's history. This crate is
//! the one place the four are distinguished, so both kernels distinguish them
//! the same way and a host test can pin it.
//!
//! # Why this and not the whole break
//!
//! The AArch64 CoW break (`akuma-exceptions`) is ~500 lines and welded to
//! `akuma-exec`: address-space owner resolution, `as_lock`, lazy-region lookup,
//! `CLONE_VM` thread-group handling. Almost all of that complexity is
//! *multi-thread races*, and none of it is the decision — which is scattered
//! across four separate `cow_ref_get(pa) == 0` tests that never state the rule
//! in one place. The mechanism stays with each kernel; only the judgement moves.
//!
//! # `marked` is not `refs > 0`
//!
//! [`CowFault::marked`] is a **property of the page-table entry**, not of the
//! share count. It has to be, and this is the trap
//! `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` is about: a read-only page
//! and a CoW-demoted page look *identical* in the PTE unless something marks
//! one of them. Deciding from the refcount alone means an `mprotect(PROT_READ)`
//! page whose frame happens to be shared gets silently promoted to writable.
//!
//! amd64 carries the mark in a spare PTE bit. **AArch64 does not have one
//! today** — it tests `cow_ref_get(pa) > 0` and lives with the ambiguity — so
//! an adapter there passes `marked: refs > 0`. That is a **pinned divergence**,
//! recorded here rather than hidden: it is why the AArch64 kernel cannot
//! currently tell those two cases apart, and adding a marker bit there is the
//! fix if it ever bites.

#![no_std]
#![forbid(unsafe_code)]

/// What the fault handler knows when a write fault lands on a user page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CowFault {
    /// The **live** PTE already grants this write.
    ///
    /// Read the entry again inside the handler rather than trusting the fault:
    /// on any kernel where a second thread can repair the page between the trap
    /// and the handler, a stale read makes a legal write look like a violation.
    /// That is the `cowstale` class — `[WPF] … ap_rw=true cow_ref=0` — and it
    /// killed real processes before the re-check was added.
    pub pte_writable: bool,
    /// The PTE marks this page copy-on-write. See the module header on why this
    /// is not `refs > 0`.
    pub marked: bool,
    /// How many address spaces share the frame, from the PMM's table. `0` means
    /// untracked, which the PMM defines as a single owner.
    pub refs: u16,
}

/// What the handler should do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CowAction {
    /// Return and re-execute. The page table already permits the write — the
    /// fault was taken before someone else repaired the page and is being judged
    /// after. **Not** an error, and killing the process here is the bug.
    Retry,
    /// Not a copy-on-write page: a genuine protection violation. The caller
    /// continues to whatever it does for a bad write (`SIGSEGV`).
    Fault,
    /// The last holder. Clear the marker and grant the write **in place** — no
    /// allocation, no copy.
    ///
    /// This is not merely an optimisation. The overwhelmingly common shape is
    /// `fork` immediately followed by `execve`: the child touches a handful of
    /// pages and then throws the whole address space away, and the parent is
    /// left as sole owner of everything it wrote. Copying there is pure waste,
    /// and on a target whose `fork` is the memory bottleneck it is the
    /// difference that matters.
    TakeInPlace,
    /// Genuinely shared. Allocate a frame, copy the page, map the copy writable
    /// and private, and release this address space's reference to the old one.
    Copy,
}

impl CowFault {
    /// Decide what a write fault on a read-only user page means.
    ///
    /// The order of the tests is the substance:
    ///
    /// 1. **`pte_writable` first**, before anything else looks at CoW state. A
    ///    page that already grants the write needs no decision, and every other
    ///    arm would be reasoning about a page that is no longer in the state
    ///    that faulted.
    /// 2. **`marked` before `refs`.** An unmarked page is not CoW however many
    ///    address spaces happen to share its frame — that is the
    ///    grant-vs-deny-records rule, and inverting these two is how
    ///    `mprotect(PROT_READ)` stops working across a `fork`.
    /// 3. **`refs <= 1` is sole ownership.** `0` is the PMM's "untracked, single
    ///    owner"; `1` is tracked with one holder — this one. Both mean nobody
    ///    else can observe the page, so it can be taken in place.
    #[must_use]
    pub const fn decide(&self) -> CowAction {
        if self.pte_writable {
            return CowAction::Retry;
        }
        if !self.marked {
            return CowAction::Fault;
        }
        if self.refs <= 1 {
            return CowAction::TakeInPlace;
        }
        CowAction::Copy
    }
}

impl CowAction {
    /// Does this action leave the page writable and the faulting instruction
    /// safe to re-execute?
    #[must_use]
    pub const fn resolves(self) -> bool {
        matches!(self, Self::Retry | Self::TakeInPlace | Self::Copy)
    }

    /// Does this action need a fresh frame from the allocator?
    ///
    /// Exactly one does, which is what makes an out-of-memory arm easy to place:
    /// only [`Self::Copy`] can fail for want of memory.
    #[must_use]
    pub const fn needs_frame(self) -> bool {
        matches!(self, Self::Copy)
    }
}

#[cfg(test)]
mod tests;
