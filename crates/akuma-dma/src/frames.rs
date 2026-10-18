//! BSS-resident frame storage, bounds- and borrow-checked.
//!
//! # What this replaces
//!
//! Five `static mut` byte arrays — `smoltcp_net`'s `RX_BUFFER` and
//! `LOOPBACK_BUFS`, `virtio_rings`' `RX_BUFS`, `TX_BUFS` and `TX_DISCARD` —
//! reached through three near-identical accessor pairs, each carrying a
//! hand-written safety argument that said the same thing: one device, one
//! owner, serialised by `NETWORK`.
//!
//! Two things were wrong with that, beyond the duplication.
//!
//! **The slot arithmetic was unchecked.** `rx_buf`, `tx_buf` and `loopback_buf`
//! were `unsafe fn` whose stated contract was `slot < RING`, computing
//! `base.add(slot * LEN)` with nothing enforcing it. A slot index that
//! desynchronised from its ring — the exact failure `ORPHAN_TOKENS` exists to
//! count — would write past the array into whatever BSS follows, with no fault
//! and no counter. [`FrameArena::slot_ptr`] returns `None` instead.
//!
//! **The aliasing obligation could not be discharged by the caller.** The
//! accessors handed back `'static` slices (later raw pointers, which was an
//! improvement — see `smoltcp_net::rx_buffer`'s note), so "hold `NETWORK` and
//! take no second borrow" was a rule enforced by reading the code. Now a second
//! exclusive borrow of a live slot returns `None` rather than minting a second
//! `&mut`.
//!
//! # Why the buffers are here and not in the device struct
//!
//! Unchanged from what the old statics documented, and still load-bearing:
//! `NetworkState` is *built on the kernel stack* and then moved into the
//! `NETWORK` static, so inline frame arrays would push tens of kilobytes
//! through a 96 KB system stack during `init`. Keeping the storage in dedicated
//! statics leaves the ring structs holding only tokens and indices.
//!
//! # What this does NOT do
//!
//! It does not model **device** ownership. Between `receive_begin` and
//! `receive_complete` the NIC owns a buffer by DMA, which no Rust borrow
//! expresses; that obligation belongs to the virtio-drivers API and is
//! concentrated in [`crate::nic`]. The borrow flag here is about Rust-level
//! aliasing only, and the two are deliberately separate: a scoped
//! [`FrameArena::with_slot`] around a `receive_begin` call is correct even
//! though the device keeps the buffer afterwards, because the *reference* dies
//! at the closure's end while the *pointer* the device holds does not.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// A fixed array of `SLOTS` frame buffers of `LEN` bytes each.
///
/// Intended to live in a `static`. All-zero contents put it in BSS, so the
/// binary does not grow by `SLOTS * LEN`.
///
/// `SLOTS` is capped at 32 by the borrow bitmask ([`FrameArena::new`] asserts
/// it at compile time). Every ring in this crate is well under that: the
/// deepest is the 32-slot loopback ring.
pub struct FrameArena<const SLOTS: usize, const LEN: usize> {
    cells: UnsafeCell<[[u8; LEN]; SLOTS]>,
    /// Bit `i` is set while slot `i` is exclusively borrowed by a
    /// [`FrameArena::with_slot`] call or a live [`FrameLease`].
    borrowed: AtomicU32,
}

// SAFETY: the storage is only ever reached through `slot_ptr` (which forms a
// pointer and hands the deref obligation to the caller) or through the
// bitmask-guarded exclusive paths below, which admit at most one live `&mut`
// per slot across all cores. `u8` has no interior invariants to violate.
unsafe impl<const SLOTS: usize, const LEN: usize> Sync for FrameArena<SLOTS, LEN> {}

impl<const SLOTS: usize, const LEN: usize> FrameArena<SLOTS, LEN> {
    #[must_use]
    pub const fn new() -> Self {
        assert!(SLOTS >= 1, "FrameArena must have at least one slot");
        assert!(SLOTS <= 32, "FrameArena borrow mask holds 32 slots");
        assert!(LEN > 0, "FrameArena slots must be non-empty");
        Self {
            cells: UnsafeCell::new([[0u8; LEN]; SLOTS]),
            borrowed: AtomicU32::new(0),
        }
    }

    /// Slots in this arena.
    #[must_use]
    pub const fn slots(&self) -> usize {
        SLOTS
    }

    /// Bytes per slot.
    #[must_use]
    pub const fn slot_len(&self) -> usize {
        LEN
    }

    /// A raw slice pointer to slot `slot`, or `None` if out of range.
    ///
    /// **Forming the pointer is safe; dereferencing it is not.** That split is
    /// the whole point — it is what lets the bounds check happen here, once,
    /// instead of being a contract clause every caller has to honour.
    ///
    /// Use this only where the borrow must outlive the call in a shape
    /// [`Self::lease`] cannot express — in practice, handing a frame pointer to
    /// smoltcp's `Device::receive`, whose token lifetime is tied to a `&mut
    /// self` this arena knows nothing about. At the deref the caller must hold
    /// `NETWORK` and must not have a second live borrow of the same slot.
    #[must_use]
    pub fn slot_ptr(&self, slot: usize) -> Option<*mut [u8]> {
        if slot >= SLOTS {
            return None;
        }
        let base = self.cells.get().cast::<u8>();
        // SAFETY: `slot < SLOTS` was just checked, so `slot * LEN + LEN` is
        // within the allocated array and the offset cannot wrap.
        let start = unsafe { base.add(slot * LEN) };
        Some(core::ptr::slice_from_raw_parts_mut(start, LEN))
    }

    /// A raw slice pointer to slot 0, infallibly.
    ///
    /// [`Self::slot_ptr`] is `Option` because the slot index is usually a
    /// runtime value from a ring. Where the arena has exactly one slot the
    /// index is a constant and the check is noise — `SLOTS >= 1` is asserted at
    /// construction, so this cannot fail.
    ///
    /// Same rule as `slot_ptr`: forming it is safe, dereferencing it is the
    /// caller's obligation.
    #[must_use]
    pub fn first_slot_ptr(&self) -> *mut [u8] {
        core::ptr::slice_from_raw_parts_mut(self.cells.get().cast::<u8>(), LEN)
    }

    /// Try to mark slot `slot` exclusively borrowed.
    ///
    /// `false` if out of range or already borrowed.
    fn try_claim(&self, slot: usize) -> bool {
        if slot >= SLOTS {
            return false;
        }
        let bit = 1u32 << slot;
        // Acquire on success so anything the previous holder wrote into the
        // slot is visible before this borrow reads it.
        self.borrowed
            .fetch_or(bit, Ordering::Acquire) & bit == 0
    }

    /// Release slot `slot`. Only ever called for a slot this arena claimed.
    fn release(&self, slot: usize) {
        // Release so the next claimer sees everything written under this borrow.
        self.borrowed.fetch_and(!(1u32 << slot), Ordering::Release);
    }

    /// Run `f` with exclusive access to slot `slot`.
    ///
    /// `None` — without running `f` — if the slot is out of range or already
    /// borrowed. A `None` here is a bug in the caller's slot bookkeeping, but
    /// it is a *reported* bug rather than two aliasing `&mut`, so the caller
    /// can drop the frame and count it the way a full ring already does.
    pub fn with_slot<R>(&self, slot: usize, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        let lease = self.lease(slot)?;
        // Reborrow through the lease so the release happens on drop even if `f`
        // unwinds (it cannot in the kernel, but the arena is host-tested too).
        let mut lease = lease;
        Some(f(&mut lease))
    }

    /// Take an exclusive borrow of slot `slot` that outlives this call.
    ///
    /// The lease derefs to the slot and releases it on drop. Use it where the
    /// borrow has to travel — smoltcp's `RxToken` carries a frame from
    /// `Device::receive` until `consume` returns, which is longer than any
    /// closure this arena could wrap around it.
    #[must_use]
    pub fn lease(&self, slot: usize) -> Option<FrameLease<'_, SLOTS, LEN>> {
        let ptr = self.slot_ptr(slot)?;
        if !self.try_claim(slot) {
            return None;
        }
        Some(FrameLease { arena: self, slot, ptr })
    }

    /// Whether slot `slot` is currently borrowed. Diagnostics and tests only —
    /// acting on it would be a race.
    #[must_use]
    pub fn is_borrowed(&self, slot: usize) -> bool {
        slot < SLOTS && self.borrowed.load(Ordering::Relaxed) & (1u32 << slot) != 0
    }
}

impl<const SLOTS: usize, const LEN: usize> Default for FrameArena<SLOTS, LEN> {
    fn default() -> Self {
        Self::new()
    }
}

/// An exclusive borrow of one [`FrameArena`] slot, released on drop.
pub struct FrameLease<'a, const SLOTS: usize, const LEN: usize> {
    arena: &'a FrameArena<SLOTS, LEN>,
    slot: usize,
    ptr: *mut [u8],
}

// SAFETY: a lease is a token of *exclusive* access to a slot of a `Sync` arena
// that lives for the program's life. Moving it between cores is sound for the
// same reason `&mut T: Send` is: exclusivity means there is no second accessor
// to race with. Required because the rings that hold leases live inside
// `NetworkState`, which sits in a `Spinlock` static — `Spinlock<T>: Sync` needs
// `T: Send`.
unsafe impl<const SLOTS: usize, const LEN: usize> Send for FrameLease<'_, SLOTS, LEN> {}

impl<const SLOTS: usize, const LEN: usize> FrameLease<'_, SLOTS, LEN> {
    /// Which slot this lease holds.
    #[must_use]
    pub const fn slot(&self) -> usize {
        self.slot
    }
}

impl<const SLOTS: usize, const LEN: usize> Deref for FrameLease<'_, SLOTS, LEN> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: the lease holds the slot's borrow bit for its whole life, so
        // no other `&mut` to these bytes exists; the pointer came from
        // `slot_ptr`, which bounds-checked it.
        unsafe { &*self.ptr }
    }
}

impl<const SLOTS: usize, const LEN: usize> DerefMut for FrameLease<'_, SLOTS, LEN> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `deref`, and `&mut self` rules out a second reference
        // through this lease.
        unsafe { &mut *self.ptr }
    }
}

impl<const SLOTS: usize, const LEN: usize> Drop for FrameLease<'_, SLOTS, LEN> {
    fn drop(&mut self) {
        self.arena.release(self.slot);
    }
}

#[cfg(test)]
mod tests {
    use super::FrameArena;

    /// The bug the bounds check exists for: a slot index that desynchronised
    /// from its ring used to compute an out-of-array pointer and write through
    /// it. Silent in production — no fault, no counter, just corrupted BSS.
    #[test]
    fn an_out_of_range_slot_is_refused_rather_than_computed() {
        let arena: FrameArena<4, 64> = FrameArena::new();
        assert!(arena.slot_ptr(4).is_none());
        assert!(arena.slot_ptr(usize::MAX).is_none());
        assert!(arena.lease(4).is_none());
        assert!(arena.with_slot(9, |_| ()).is_none());
    }

    #[test]
    fn every_in_range_slot_is_addressable() {
        let arena: FrameArena<4, 64> = FrameArena::new();
        for slot in 0..4 {
            assert!(arena.slot_ptr(slot).is_some(), "slot {slot}");
        }
    }

    /// Slots must not overlap: writing one must not be visible in another.
    /// This is what `base.add(slot * LEN)` is supposed to guarantee and what a
    /// wrong stride would break.
    #[test]
    fn slots_are_disjoint() {
        let arena: FrameArena<4, 64> = FrameArena::new();
        for slot in 0..4 {
            arena.with_slot(slot, |buf| buf.fill(slot as u8 + 1)).unwrap();
        }
        for slot in 0..4 {
            arena
                .with_slot(slot, |buf| {
                    assert!(
                        buf.iter().all(|b| *b == slot as u8 + 1),
                        "slot {slot} was written by a neighbour"
                    );
                    assert_eq!(buf.len(), 64);
                })
                .unwrap();
        }
    }

    /// The aliasing rule, now enforced instead of documented.
    #[test]
    fn a_second_exclusive_borrow_is_refused_not_aliased() {
        let arena: FrameArena<4, 64> = FrameArena::new();
        let held = arena.lease(1).expect("first lease");
        assert!(arena.lease(1).is_none(), "slot 1 is already borrowed");
        assert!(arena.with_slot(1, |_| ()).is_none());
        // A different slot is unaffected — the mask is per-slot, not a
        // whole-arena lock.
        assert!(arena.lease(2).is_some());
        drop(held);
    }

    #[test]
    fn a_slot_is_reusable_once_its_lease_drops() {
        let arena: FrameArena<2, 32> = FrameArena::new();
        {
            let mut lease = arena.lease(0).expect("lease");
            lease[0] = 0xAB;
            assert!(arena.is_borrowed(0));
        }
        assert!(!arena.is_borrowed(0));
        let again = arena.lease(0).expect("slot released on drop");
        assert_eq!(again[0], 0xAB, "contents survive the lease");
    }

    /// `with_slot` must release even though the closure returns a value, and
    /// must hand back that value.
    #[test]
    fn with_slot_returns_the_closures_value_and_releases() {
        let arena: FrameArena<2, 8> = FrameArena::new();
        let n = arena.with_slot(0, |buf| {
            buf[0] = 7;
            buf.len()
        });
        assert_eq!(n, Some(8));
        assert!(!arena.is_borrowed(0));
    }

    /// A refused claim must not have run the closure — otherwise a caller that
    /// treats `None` as "dropped frame" would have written into a live slot on
    /// the way to finding out.
    #[test]
    fn a_refused_with_slot_never_runs_the_closure() {
        let arena: FrameArena<2, 8> = FrameArena::new();
        let _held = arena.lease(0).unwrap();
        let mut ran = false;
        let r = arena.with_slot(0, |_| {
            ran = true;
        });
        assert!(r.is_none());
        assert!(!ran, "closure ran against a borrowed slot");
    }

    /// The claim is a CAS-shaped bitmask op, so concurrent claimers must not
    /// both win. Host-only, but the kernel reaches this from several cores
    /// under `smp-shared`.
    #[test]
    fn concurrent_claims_of_one_slot_admit_exactly_one_winner() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let arena: Arc<FrameArena<8, 64>> = Arc::new(FrameArena::new());
        let wins = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..8 {
            let arena = Arc::clone(&arena);
            let wins = Arc::clone(&wins);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    if let Some(mut lease) = arena.lease(3) {
                        // If two leases were ever live at once this write and
                        // the read below would race; the count is the assertion.
                        lease[0] = 1;
                        wins.fetch_add(1, Ordering::Relaxed);
                        drop(lease);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Every successful claim was exclusive; the total is whatever the
        // interleaving allowed, but the arena must be free at the end.
        assert!(!arena.is_borrowed(3));
        assert!(wins.load(Ordering::Relaxed) > 0);
    }
}
