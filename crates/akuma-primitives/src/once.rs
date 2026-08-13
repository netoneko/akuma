//! Single-shot, lock-free cells for boot-registered values.

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
/// mechanism (`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` Phase 0 item 3) — which
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
