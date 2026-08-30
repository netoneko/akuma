//! Single-shot, lock-free cells for boot-registered values.
//!
//! Three shapes, all "written once at boot, used forever after":
//! [`OnceCopy`] for a `Copy` value read from anywhere, [`Registered`] for a
//! callback table with a diagnostic, and [`TakeOnce`] for a large `static`
//! buffer that exactly one owner needs `&'static mut` to.
//!
//! # Why this is its own crate
//!
//! Split out of `akuma-primitives` on 2026-08-30. It was that crate's last five
//! `unsafe` sites and the only ones that were **not** platform-specific — the
//! other sixteen are system-register `asm!` and MMIO. Grouping a
//! `UnsafeCell<MaybeUninit<T>>` with `msr daifset` under one crate meant neither
//! could be reviewed as one idea.
//!
//! # Why it will never take a dependency
//!
//! [`Registered`] is the mechanism by which every *other* extracted crate calls
//! back up into the kernel without a dependency cycle — `akuma-bkl`'s yield hook,
//! `akuma-mmu`'s `SchedHooks`, `akuma-elf`'s `VfsHooks`, `akuma-pmm`'s
//! `PmmHooks`, `akuma-ext2`'s thread hooks. Nine crates plus the bin use it, so
//! anything this crate depended on would become a de-facto dependency of the
//! whole tree. `core` only, permanently.
//!
//! # Why the `unsafe` is irreducible
//!
//! The obvious safe alternative is `Spinlock<Option<T>>`. It would be a real
//! regression: [`Registered::get`] sits on the hottest indirection in the kernel
//! — every `runtime()` call goes through one — and it is a relaxed atomic load
//! plus a read out of an already-initialised cell. Trading that for a lock
//! acquire on every syscall's callback lookup is not a safety win worth having.
//!
//! # The name
//!
//! Not even once.

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

/// Single-shot, lock-free cell for `Copy` types.
///
/// Set once at init, then read freely from any context (including IRQ
/// handlers). No spinlock — readers must never block on writers, because
/// reading a boot-registered callback table from inside an IRQ that
/// interrupted code holding the same lock would self-deadlock on a single CPU.
///
/// This is the tree's one mechanism for "registered at boot, read from
/// anywhere". It lived in `akuma_exec::runtime` and was made `pub` there so
/// `akuma-ext2`'s thread hooks could reuse it rather than inventing a second
/// mechanism (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` Phase 0 item 3) — which
/// meant `akuma-ext2` depended on the 23.8k-line execution crate for a 40-line
/// cell. It lives here now so reuse costs nothing.
pub struct OnceCopy<T: Copy> {
    initialized: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the only write happens in `set` before the Release store to
// `initialized`; readers load it Acquire and so observe a fully-written value.
// `T: Copy` means `get` hands out a copy and never aliases the cell.
unsafe impl<T: Copy + Send + Sync> Sync for OnceCopy<T> {}

impl<T: Copy> OnceCopy<T> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Write the value. Must be called exactly once before any `get()`.
    /// Second call is silently ignored — callers shouldn't rely on that.
    pub fn set(&self, v: T) {
        if self.initialized.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: we are the only writer (single-shot at boot); readers
        // observe the value only after the Release store below.
        unsafe { (*self.value.get()).write(v) };
        self.initialized.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn get(&self) -> Option<T> {
        if self.initialized.load(Ordering::Acquire) {
            // SAFETY: initialized=true means the value was fully written
            // before the Release store; T: Copy lets us read a copy.
            Some(unsafe { (*self.value.get()).assume_init_read() })
        } else {
            None
        }
    }

    /// Whether the cell has been set. Non-panicking probe for code that may run
    /// before registration and wants to degrade rather than fail.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }
}

impl<T: Copy> Default for OnceCopy<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A boot-registered callback table: [`OnceCopy`] plus the four operations
/// every such table needs, and the diagnostic to panic with when it is read
/// before the kernel registered it.
///
/// # Why this exists
///
/// Three crates had written the same thing, and they had not agreed:
///
/// | crate | was |
/// |---|---|
/// | `akuma-exec` | `OnceCopy<ExecRuntime>` + `OnceCopy<ExecConfig>`, four accessors |
/// | `akuma-net` | **`Spinlock<Option<NetRuntime>>`**, three accessors |
/// | `akuma-ext2` | `OnceCopy<ThreadHooks>`, hand-rolled `map_or` at each read |
///
/// The `akuma-net` copy was the expensive one: it took a spinlock on *every*
/// read of the runtime table. Its own `NetRuntime` doc comment records that
/// `virt_to_phys`/`phys_to_virt` were moved out of the struct because the
/// indirection "cost a spinlocked struct read on the per-packet DMA path" —
/// while the struct itself was still reached through exactly that. It is
/// lock-free now, like the other two.
///
/// Beyond the cost, a spinlock is the *wrong* mechanism here for the reason
/// [`OnceCopy`] spells out: a callback table read from an IRQ handler that
/// interrupted the lock holder self-deadlocks on a single core.
///
/// # Registration is single-shot and idempotent
///
/// A second `register` is silently ignored (inherited from [`OnceCopy::set`]).
/// Every in-tree registration happens exactly once, from the kernel's boot
/// path. That property is also what lets **host unit tests inject a table
/// unconditionally** from parallel test threads without ordering or races —
/// see `akuma_exec::runtime::register_config_for_test`.
pub struct Registered<T: Copy> {
    cell: OnceCopy<T>,
    /// Full panic message for [`Self::require`], e.g.
    /// `"akuma-net: NetRuntime not registered — call akuma_net::init() first"`.
    /// Stored whole rather than composed so each crate keeps its exact wording.
    absent: &'static str,
}

impl<T: Copy> Registered<T> {
    #[must_use]
    pub const fn new(absent: &'static str) -> Self {
        Self {
            cell: OnceCopy::new(),
            absent,
        }
    }

    /// Publish the table. Call once, from the crate's `init`.
    pub fn register(&self, v: T) {
        self.cell.set(v);
    }

    /// The table, or `None` if the kernel has not registered it yet. For code
    /// that can run before registration and wants to degrade rather than fail
    /// — early diagnostics, boot-time instrumentation.
    #[must_use]
    pub fn get(&self) -> Option<T> {
        self.cell.get()
    }

    /// The table, panicking with this cell's diagnostic if absent. The default
    /// accessor: everything past `init` should use it, because a missing
    /// callback table there is a boot-order bug, not a condition to handle.
    #[must_use]
    pub fn require(&self) -> T {
        match self.cell.get() {
            Some(v) => v,
            None => panic!("{}", self.absent),
        }
    }

    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.cell.is_set()
    }
}

#[cfg(test)]
mod registered_tests {
    use super::Registered;

    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Hooks {
        tick: fn() -> u64,
    }

    fn forty_two() -> u64 {
        42
    }

    #[test]
    fn absent_until_registered_then_readable() {
        static R: Registered<Hooks> = Registered::new("test: not registered");
        assert!(!R.is_registered());
        assert!(R.get().is_none());

        R.register(Hooks { tick: forty_two });

        assert!(R.is_registered());
        assert_eq!((R.require().tick)(), 42);
        assert_eq!((R.get().unwrap().tick)(), 42);
    }

    /// Single-shot: a second `register` is ignored, not last-writer-wins. This
    /// is what makes it safe for parallel host tests to inject unconditionally,
    /// and it is the semantic `akuma-net` changed to when it stopped using
    /// `Spinlock<Option<_>>` — sound there because its two `register` sites are
    /// mutually exclusive cfgs, called once from boot.
    #[test]
    fn registration_is_single_shot_and_idempotent() {
        fn seven() -> u64 {
            7
        }
        static R: Registered<Hooks> = Registered::new("test: not registered");

        R.register(Hooks { tick: forty_two });
        R.register(Hooks { tick: seven });

        assert_eq!((R.require().tick)(), 42, "second register must not win");
    }

    /// `require()` panics with the cell's own diagnostic, so the message names
    /// the crate and the init call the caller forgot.
    #[test]
    #[should_panic(expected = "akuma-test: Hooks not registered — call init() first")]
    fn require_panics_with_the_registered_diagnostic() {
        static R: Registered<Hooks> =
            Registered::new("akuma-test: Hooks not registered — call init() first");
        let _ = R.require();
    }
}

#[cfg(test)]
mod tests {
    use super::OnceCopy;

    #[test]
    fn get_returns_none_before_set() {
        let cell: OnceCopy<u32> = OnceCopy::new();
        assert!(cell.get().is_none());
        assert!(!cell.is_set());
    }

    #[test]
    fn get_returns_value_after_set() {
        let cell: OnceCopy<u32> = OnceCopy::new();
        cell.set(0xc0ffee);
        assert_eq!(cell.get(), Some(0xc0ffee));
        assert!(cell.is_set());
    }

    #[test]
    fn second_set_is_ignored() {
        let cell: OnceCopy<u32> = OnceCopy::new();
        cell.set(1);
        cell.set(2);
        assert_eq!(cell.get(), Some(1));
    }

    #[test]
    fn many_reads_return_same_value() {
        let cell: OnceCopy<u64> = OnceCopy::new();
        cell.set(0xdead_beef_cafe_babe);
        for _ in 0..10_000 {
            assert_eq!(cell.get(), Some(0xdead_beef_cafe_babe));
        }
    }

    #[test]
    fn concurrent_readers_after_set_never_block() {
        // Lock-free contract: many threads reading concurrently must each
        // observe the value with no spinning, no panics. If anyone ever
        // reintroduced a Spinlock-on-read, this would still pass (no
        // contention), but combined with the "called from IRQ" kernel
        // test it nails down the invariant.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let cell: Arc<OnceCopy<u32>> = Arc::new(OnceCopy::new());
        cell.set(42);

        let hits = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..8 {
            let cell = Arc::clone(&cell);
            let hits = Arc::clone(&hits);
            handles.push(thread::spawn(move || {
                for _ in 0..10_000 {
                    if cell.get() == Some(42) {
                        hits.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(hits.load(Ordering::Relaxed), 8 * 10_000);
    }
}


/// A `static` value that exactly one caller may take a `&'static mut` to.
///
/// # Why this exists
///
/// The alternative is `static mut BUF: [T; N]` plus `unsafe { &mut BUF }` at
/// the one place that claims it, and the `unsafe` there is load-bearing for a
/// reason nothing in the code can check: it is sound **only** because the
/// caller is the sole claimer, which is a property of the whole program rather
/// than of that statement. A second claim — added later, or on a second core —
/// is instant UB with no diagnostic.
///
/// `TakeOnce` makes the claim itself the check. The first [`Self::take`] hands
/// back the reference; every later one gets `None`. That converts "there is
/// only one caller" from a comment into something the type enforces, and the
/// call site stops being `unsafe` at all.
///
/// Used for buffers too large to live in a struct that is built on the kernel
/// stack — smoltcp's `SocketStorage` table is the motivating case
/// (`akuma_net::smoltcp_net::init`). For frame buffers that need *repeated*
/// scoped borrows rather than one permanent one, see `akuma_net::frames`.
pub struct TakeOnce<T> {
    taken: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `take` hands out at most one `&'static mut T`, gated by an atomic
// swap, so no two callers can hold overlapping references. `T: Send` is
// required because the single reference may be claimed on any core.
unsafe impl<T: Send> Sync for TakeOnce<T> {}

impl<T> TakeOnce<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            taken: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Claim the value. `Some` exactly once for the life of the program.
    ///
    /// The `&'static` bound on `self` is what makes the returned lifetime
    /// honest: only a genuine `static` can hand out a `'static` borrow.
    ///
    /// `mut_from_ref` is allowed because the shared-in/mutable-out signature is
    /// the whole mechanism, not an oversight: the atomic swap is what makes at
    /// most one such reference exist, and taking `&mut self` instead would need
    /// a caller who already had exclusive access to the `static` — the exact
    /// thing this type is here to avoid.
    #[allow(clippy::mut_from_ref)]
    pub fn take(&'static self) -> Option<&'static mut T> {
        if self.taken.swap(true, Ordering::AcqRel) {
            return None;
        }
        // SAFETY: the swap above admits exactly one caller past this point for
        // the life of the program, so this is the only reference that will ever
        // exist to the cell's contents.
        Some(unsafe { &mut *self.value.get() })
    }

    /// Whether the value has been claimed.
    #[must_use]
    pub fn is_taken(&self) -> bool {
        self.taken.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod take_once_tests {
    use super::TakeOnce;

    #[test]
    fn the_first_take_wins_and_the_rest_get_none() {
        static CELL: TakeOnce<[u32; 4]> = TakeOnce::new([0; 4]);
        assert!(!CELL.is_taken());
        let first = CELL.take().expect("first take");
        first[0] = 7;
        assert!(CELL.is_taken());
        assert!(CELL.take().is_none(), "a second &mut would be UB");
        assert!(CELL.take().is_none());
        assert_eq!(first[0], 7);
    }

    /// The property that matters under SMP: concurrent claimers, exactly one
    /// winner. A `static mut` + `unsafe { &mut }` had no such guarantee.
    #[test]
    fn concurrent_takes_admit_exactly_one_winner() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static CELL: TakeOnce<[u8; 16]> = TakeOnce::new([0; 16]);
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..8 {
            let winners = Arc::clone(&winners);
            handles.push(std::thread::spawn(move || {
                if CELL.take().is_some() {
                    winners.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::Relaxed), 1);
    }
}
