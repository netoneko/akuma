//! The copy-on-write break and the W^X record, over the machine's entry bits.
//!
//! The decisions are `akuma-cow`'s (which of retry / fault / take in place /
//! copy, and whether a kernel store may land) and `akuma-syscalls-mem::wx`'s.
//! What this adds is carrying them out on a live leaf: re-read it, ask, edit
//! it, invalidate, and order the shootdown before the old frame's count drops.
//! Every bit it reads or writes is named by [`UserPages`](crate::mmu::UserPages),
//! so the same code breaks a share on amd64 (`RW`, bit-9 `COW`, `D`) and on
//! AArch64 (its own encoding).
//!
//! Every caller holds the kernel lock (a trap or a syscall): nothing races the walk.

use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use akuma_cow::{CowAction, KernelWrite};

use crate::mm::Mm;
use crate::mmu::FreshPage;

// W^X (`akuma_syscalls_mem::wx`) is decided in `mm.rs` and has no state here: no page is writable and executable, and
// a page written since it was mapped cannot be made executable. Always on - there is no switch (the `wx=0`
// command-line hatch was removed 2026-11-05: production kernels do not run a JIT). Stricter than Linux on purpose.

static COW_SHARED: AtomicUsize = AtomicUsize::new(0);
static COW_COPY: AtomicUsize = AtomicUsize::new(0);
static COW_TAKE: AtomicUsize = AtomicUsize::new(0);
static COW_RETRY: AtomicUsize = AtomicUsize::new(0);

/// A fork shared one more page (the clone's count).
pub fn count_shared() {
    COW_SHARED.fetch_add(1, Relaxed);
}

/// `(shared, copied, taken-in-place, retried)` since boot, for the reap lines.
#[must_use]
pub fn stats() -> (usize, usize, usize, usize) {
    (COW_SHARED.load(Relaxed), COW_COPY.load(Relaxed), COW_TAKE.load(Relaxed), COW_RETRY.load(Relaxed))
}

/// The PMM share count of the frame a valid entry names (0 for an invalid one).
fn share_count<M: Mm>(pte: u64) -> u16 {
    M::frame_of(pte).map_or(0, |pa| akuma_pmm::cow_ref_get(pa as usize))
}

/// Has the page at `va` in `root` been written since it was mapped (user store or kernel store)?
#[must_use]
pub fn page_written<M: Mm>(root: u64, va: u64) -> bool {
    M::leaf(root, va & !0xFFF).is_some_and(M::written)
}

/// Record that the kernel stored into the user page at `va`: the CPU records a
/// user store, nothing records ours.
pub fn mark_written<M: Mm>(root: u64, va: u64) {
    let va = va & !0xFFF;
    if let Some(pte) = M::leaf(root, va)
        && M::frame_of(pte).is_some()
        && !M::written(pte)
    {
        M::write_leaf(root, va, M::with_written(pte));
    }
}

/// THE copy-on-write break: re-read the live entry for `va` in `root`, ask
/// `akuma_cow`, and carry the answer out.
///
/// The write-fault arm and the kernel's own user-memory writers both come
/// through here - one break path. An invalidate after every entry write is this
/// core's whole shootdown; a copy also shoots down peers before the old frame's
/// count drops.
pub fn resolve<M: Mm>(root: u64, va: u64) -> CowAction {
    let va = va & !0xFFF;
    let Some(pte) = M::leaf(root, va) else {
        return CowAction::Fault;
    };
    let Some(old) = M::frame_of(pte) else {
        return CowAction::Fault;
    };
    let action =
        akuma_cow::CowFault { pte_writable: M::writable(pte), marked: M::cow_marked(pte), refs: share_count::<M>(pte) }
            .decide();
    match action {
        CowAction::Retry => {
            COW_RETRY.fetch_add(1, Relaxed);
            M::invalidate(va);
        }
        CowAction::Fault => {}
        CowAction::TakeInPlace => {
            M::write_leaf(root, va, M::take(pte));
            M::invalidate(va);
            COW_TAKE.fetch_add(1, Relaxed);
        }
        CowAction::Copy => {
            let Some(mut new) = M::Frame::alloc() else {
                // Out of frames: the caller's kill path reports it.
                return CowAction::Fault;
            };
            if !akuma_mmu::copy_from_phys(old as usize, new.bytes_mut()) {
                return CowAction::Fault; // `new` drops back to the PMM
            }
            let new = new.publish() as u64;
            M::write_leaf(root, va, M::take_at(pte, new));
            M::invalidate(va);
            // A peer running a thread of this space may still map the old
            // frame: tell it before the old frame's count drops.
            M::shootdown();
            // Drops this space's count on the old frame (frees it only if it
            // was somehow the last holder).
            akuma_pmm::free_page(old as usize, akuma_primitives::preempt::current_tid() as u32);
            COW_COPY.fetch_add(1, Relaxed);
        }
    }
    action
}

/// Before the kernel writes into the user page at `va` (through the physmap,
/// which never faults): may the write happen? `false` is `EFAULT`, as on Linux.
///
/// * A marked read-only page is a CoW share: break it first. If the break
///   cannot get a frame the write is refused - attempted anyway, it would land
///   in the still-shared frame and corrupt the other holder.
/// * An UNMARKED read-only page that several spaces hold (shared text, or an
///   `mprotect(PROT_READ)` page after a fork) must not be written: the write
///   would land in every holder. Refused.
/// * An unshared read-only page is left writable to the kernel (the only holder
///   is the one asking).
#[must_use]
pub fn prepare_write<M: Mm>(root: u64, va: u64) -> bool {
    let Some(pte) = M::leaf(root, va & !0xFFF) else {
        return true; // not mapped: the translate that follows reports it
    };
    match akuma_cow::kernel_write(M::frame_of(pte).is_some(), M::writable(pte), M::cow_marked(pte), share_count::<M>(pte)) {
        KernelWrite::Allow => true,
        KernelWrite::Break => !matches!(resolve::<M>(root, va), CowAction::Fault),
        KernelWrite::Refuse => false,
    }
}

/// `mprotect` is about to make `va` writable.
///
/// A frame several spaces hold (marked or not - a shared read-only text page is
/// unmarked) must be private first, or the write lands in every holder. `false`
/// when no frame could be had.
#[must_use]
pub fn privatize<M: Mm>(root: u64, va: u64) -> bool {
    let Some(pte) = M::leaf(root, va) else {
        return true;
    };
    if !akuma_cow::needs_private_copy(M::frame_of(pte).is_some(), share_count::<M>(pte)) {
        return true;
    }
    // Force the Copy arm for a share the entry does not mark.
    M::write_leaf(root, va, M::share_marked(pte));
    !matches!(resolve::<M>(root, va), CowAction::Fault)
}
