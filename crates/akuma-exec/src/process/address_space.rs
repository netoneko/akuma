//! [`ProcAddressSpace`] — a process's [`UserAddressSpace`] merged with the lock
//! that serializes hardware page-table mutation on it.
//!
//! ## Why this type exists
//!
//! `Process` used to carry the address space as **two** fields:
//!
//! ```ignore
//! pub address_space: UserAddressSpace,   // the data
//! pub as_lock: Spinlock<()>,             // ...and a lock guarding nothing
//! ```
//!
//! There is no way to obtain `&mut` from a `Spinlock<()>`, so
//! `Process::with_address_space` reached around it with
//! `&mut *(addr_of!(self.address_space) as *mut UserAddressSpace)` — a `&mut`
//! synthesised from `&self` to a field that is not in an `UnsafeCell`, which is
//! UB under the aliasing model no matter how well `as_lock` serializes the
//! writers (a lock provides exclusion, not provenance). Those were the last two
//! of the six `&self -> &mut` casts in `docs/archive/AKUMA_EXEC_AUDIT.md` §5;
//! the other four were removed the same way the audit's §5c-bis removed
//! `mmap_regions`/`free_regions`: put the data **inside** the lock.
//!
//! ## The scalar mirror
//!
//! The obstacle §5-bis flagged is that four getters — `l0_phys`, `asid`,
//! `ttbr0`, `is_shared` — are read **lock-free on the page-fault fast path**
//! (~60 call sites). Wrapping the address space in a plain `Spinlock` would put
//! a spinlock acquire on every one of them, widening the `as_lock` deadlock
//! surface the field doc on `Process` already warns about.
//!
//! So this type keeps those four values as a lock-free atomic mirror
//! (`ttbr0` packs `(asid << 48) | l0_phys`; `shared` is its own bool). They are
//! **immutable in `UserAddressSpace` after construction** — the only event that
//! changes them for a live `Process` is `execve`, which calls [`replace`] to
//! swap the inner value and refresh the mirror together. Every *other*
//! `UserAddressSpace` operation goes through [`lock`], which is `as_lock` held
//! with IRQs masked on `kernel_smp_shared` (a plain spinlock hold elsewhere).
//!
//! [`replace`]: ProcAddressSpace::replace
//! [`lock`]: ProcAddressSpace::lock

use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spinning_top::guard::SpinlockGuard;
use spinning_top::Spinlock;

use crate::mmu::UserAddressSpace;

/// Bits [47:12] of a TTBR0 value — the L0 table's physical base.
const L0_BASE_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// A process's user address space plus a lock-free mirror of its scalar
/// identity. See the module docs.
pub struct ProcAddressSpace {
    /// `as_lock` + the address space it guards, as one lock-carries-data field.
    inner: Spinlock<UserAddressSpace>,
    /// `(asid as u64) << 48 | l0_phys` — the `TTBR0_EL1` value. Mirrored out of
    /// `inner` so the fault-path getters never take `inner`'s lock.
    ttbr0: AtomicU64,
    /// Mirror of `UserAddressSpace::is_shared()`.
    shared: AtomicBool,
}

impl ProcAddressSpace {
    /// Wrap a freshly built address space, seeding the scalar mirror from it.
    pub fn new(uas: UserAddressSpace) -> Self {
        Self {
            ttbr0: AtomicU64::new(uas.ttbr0()),
            shared: AtomicBool::new(uas.is_shared()),
            inner: Spinlock::new(uas),
        }
    }

    // ── lock-free scalar mirror (page-fault fast path) ──────────────────────

    /// The `TTBR0_EL1` value for this address space: `(asid << 48) | l0_phys`.
    #[inline]
    pub fn ttbr0(&self) -> u64 {
        self.ttbr0.load(Ordering::Relaxed)
    }

    /// Physical base of the L0 translation table.
    #[inline]
    pub fn l0_phys(&self) -> usize {
        (self.ttbr0.load(Ordering::Relaxed) & L0_BASE_MASK) as usize
    }

    /// The ASID assigned to this address space.
    #[inline]
    pub fn asid(&self) -> u16 {
        (self.ttbr0.load(Ordering::Relaxed) >> 48) as u16
    }

    /// Whether this is a shared view of another address space's L0
    /// (`CLONE_VM` / `vfork`), which owns none of the page frames.
    #[inline]
    pub fn is_shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    // ── locked access to the address space itself ──────────────────────────

    /// Take `as_lock` (with IRQs masked on `kernel_smp_shared`) and return a
    /// guard that derefs to the [`UserAddressSpace`].
    ///
    /// This is what `Process::with_address_space` / the old `AsLockHold` handed
    /// out — the same discipline applies: keep the hold short (PTE edits, frame
    /// bookkeeping, TLB flushes), and do **not** allocate frames, do block I/O,
    /// or yield while it is held.
    #[inline]
    pub fn lock(&self) -> AddressSpaceGuard<'_> {
        #[cfg(kernel_smp_shared)]
        let irq = crate::runtime::IrqGuard::new();
        AddressSpaceGuard {
            guard: self.inner.lock(),
            #[cfg(kernel_smp_shared)]
            _irq: irq,
        }
    }

    /// `true` if the lock is currently free (test/diagnostic leak check —
    /// takes and immediately drops the guard).
    #[inline]
    pub fn is_unlocked(&self) -> bool {
        self.inner.try_lock().is_some()
    }

    /// `&mut` to the inner address space without locking — sound because
    /// `&mut self` is proof no other reference exists. For the `&mut Process`
    /// build paths (fork child construction, `replace_image`) where the
    /// `Process` is not yet published and nothing can contend.
    #[inline]
    pub fn get_mut(&mut self) -> &mut UserAddressSpace {
        self.inner.get_mut()
    }

    // ── one-shot passthroughs ─────────────────────────────────────────────
    //
    // Each takes the lock for the single operation and drops it. Callers that
    // need several operations under ONE hold (the fault path's
    // `map_user_page` + `track_*` sequence) take `lock()` directly instead —
    // see `src/exceptions.rs`.

    /// Install this address space in `TTBR0_EL1`. See
    /// [`UserAddressSpace::activate`].
    #[inline]
    pub fn activate(&self) {
        self.inner.lock().activate();
    }

    /// [`UserAddressSpace::translate`].
    #[inline]
    pub fn translate(&self, va: usize) -> Option<usize> {
        self.inner.lock().translate(va)
    }

    /// [`UserAddressSpace::is_mapped`].
    #[inline]
    pub fn is_mapped(&self, va: usize) -> bool {
        self.inner.lock().is_mapped(va)
    }

    /// [`UserAddressSpace::is_range_mapped`].
    #[inline]
    pub fn is_range_mapped(&self, va_start: usize, len: usize) -> bool {
        self.inner.lock().is_range_mapped(va_start, len)
    }

    /// [`UserAddressSpace::resident_pages`].
    #[inline]
    pub fn resident_pages(&self) -> usize {
        self.inner.lock().resident_pages()
    }

    /// [`UserAddressSpace::tracks_user_frame`].
    #[inline]
    pub fn tracks_user_frame(&self, pa: usize) -> bool {
        self.inner.lock().tracks_user_frame(pa)
    }

    /// [`UserAddressSpace::user_frame_count`].
    #[inline]
    pub fn user_frame_count(&self) -> usize {
        self.inner.lock().user_frame_count()
    }

    /// [`UserAddressSpace::user_frame_total_refs`].
    #[inline]
    pub fn user_frame_total_refs(&self) -> usize {
        self.inner.lock().user_frame_total_refs()
    }

    /// [`UserAddressSpace::page_table_frame_count`].
    #[inline]
    pub fn page_table_frame_count(&self) -> usize {
        self.inner.lock().page_table_frame_count()
    }

    /// [`UserAddressSpace::read_l3_page_entry`].
    #[inline]
    pub fn read_l3_page_entry(&self, va: usize) -> Option<u64> {
        self.inner.lock().read_l3_page_entry(va)
    }

    /// [`UserAddressSpace::invalidate_icache_for_page_va`]. One-shot: callers
    /// that already hold [`lock`](Self::lock) across a PTE edit call the method
    /// on their guard instead (see `src/exceptions.rs`).
    #[inline]
    pub fn invalidate_icache_for_page_va(&self, va: usize) {
        self.inner.lock().invalidate_icache_for_page_va(va);
    }

    // NOTE: the *mutating* frame-tracking ops (`track_user_frame`,
    // `track_page_table_frame`, `adopt_user_frame`, `remove_user_frame`,
    // `invalidate_icache_for_page_va`) are deliberately NOT passthroughs. They
    // are called from the page-fault / CoW-break paths that already hold
    // `lock()` across a `map_user_page` + track sequence — a self-locking
    // passthrough there would deadlock. Those sites take `lock()` and go
    // through the guard.

    /// Swap in a freshly loaded address space (the `execve` core) and refresh
    /// the scalar mirror in the same step, returning the old one for the caller
    /// to drop.
    ///
    /// `&self` since `AKUMA_EXEC_AUDIT.md` §6.E group 2b — `replace_image` no
    /// longer holds `&mut Process`. The swap is done under `inner`'s lock (with
    /// IRQs masked on `kernel_smp_shared`); a lock-free `l0_phys()` reader can
    /// briefly see the new inner paired with the old `ttbr0` mirror between the
    /// two stores, but that window existed for `&mut self` too (the mirror and
    /// the inner were never updated atomically) and `execve` runs BKL-held with
    /// siblings already killed, so no such reader is live.
    pub fn replace(&self, uas: UserAddressSpace) -> UserAddressSpace {
        let ttbr0 = uas.ttbr0();
        let shared = uas.is_shared();
        let old = {
            #[cfg(kernel_smp_shared)]
            let _irq = crate::runtime::IrqGuard::new();
            core::mem::replace(&mut *self.inner.lock(), uas)
        };
        self.ttbr0.store(ttbr0, Ordering::Relaxed);
        self.shared.store(shared, Ordering::Relaxed);
        old
    }
}

/// Guard returned by [`ProcAddressSpace::lock`]. Field order matters: `guard`
/// drops before `_irq`, so the lock is released before DAIF is restored — the
/// discipline the old `AsLockHold` documented.
pub struct AddressSpaceGuard<'a> {
    guard: SpinlockGuard<'a, UserAddressSpace>,
    #[cfg(kernel_smp_shared)]
    _irq: crate::runtime::IrqGuard,
}

impl Deref for AddressSpaceGuard<'_> {
    type Target = UserAddressSpace;
    #[inline]
    fn deref(&self) -> &UserAddressSpace {
        &self.guard
    }
}

impl DerefMut for AddressSpaceGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut UserAddressSpace {
        &mut self.guard
    }
}
