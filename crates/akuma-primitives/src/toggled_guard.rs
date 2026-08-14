//! [`ToggledGuard`] — one RAII guard shape for "do a thing on entry iff a feature is
//! compiled in and a runtime toggle says so, and undo *exactly that* on every return
//! path".
//!
//! # Why this is a primitive
//!
//! Five BKL carve-out guards — `NetBklGuard`, `VfsBklGuard`, `ProcessBklGuard`,
//! `MmBklGuard`, `DriverBklGuard`, one per phase of
//! `docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md` — were each copy-pasted from the
//! one before it. Same struct, same `new`, same `Drop`, same cfg shape, and the same
//! latching rule restated in four doc comments; only the cfg predicate and the toggle
//! function differed. Five copies of the most consequential lock discipline in the
//! tree is exactly the shape §5.5 of
//! `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` flagged. The five names
//! survive as type aliases — `DriverBklGuard` still reads as `DriverBklGuard` at every
//! call site — because the name is the useful part; the body was the duplicated part.
//!
//! It lives *here*, in the leaf, rather than next to the BKL, because it contains no
//! BKL: the two actions are supplied by the implementor. Nothing in this module names
//! a lock, and the leaf-crate rule ("no dependencies, ever") is why `akuma-exec` and
//! the bin crate can both reach it.
//!
//! # The latching rule, stated once
//!
//! [`GuardToggle::enabled`] is read **once**, in `new()`, and the answer is latched in
//! the guard. `drop()` must never re-read it. For the BKL carve-outs the toggles
//! genuinely are flipped while guards are live — the A/B boot self-tests flip them
//! between phases, and they double as kill switches — and a guard that asked twice
//! would, on an ON→OFF flip mid-syscall, leave the BKL released for the rest of the
//! call. The syscall wrapper's single `leave_kernel` would then advance `now_serving`
//! for a ticket nobody owns and corrupt the FIFO for every core. Latching makes the
//! guard balanced by construction, whatever the toggle does in between.
//! (`docs/archive/BKL_VFS_CARVE_OUT.md` §2.4.)

use core::marker::PhantomData;

/// The gates and the two actions for one [`ToggledGuard`].
///
/// Implement it on an (ideally uninhabited) marker type naming the thing being
/// toggled, then alias the guard: `pub type DriverBklGuard = ToggledGuard<DriverBkl>;`
pub trait GuardToggle {
    /// Whether this guard is compiled in at all — normally
    /// `cfg!(all(kernel_smp_shared, kernel_no_bkl_…))`.
    ///
    /// When `false`, [`enter`](Self::enter) and [`exit`](Self::exit) are never reached
    /// and the guard folds to nothing: `new()` stores a constant `false` to a local
    /// nothing reads, and `drop()` is `if false {}`. That is what keeps a build
    /// without the feature unchanged.
    const COMPILED_IN: bool;

    /// The runtime toggle — the A/B handle and kill switch. Read once per guard, at
    /// construction; see the module header for why `drop` must not re-read it.
    /// Constant `true` for a guard with no runtime toggle (`no-bkl-network`).
    fn enabled() -> bool;

    /// Run on construction, only when both [`Self::COMPILED_IN`] and
    /// [`Self::enabled`] hold.
    fn enter();

    /// Undo [`Self::enter`]. Run on drop for exactly the guards whose `new()` called
    /// it, and for no others.
    fn exit();
}

/// RAII guard for a [`GuardToggle`]: enters on construction, exits on every return
/// path — including `?` early-returns — and never asks the toggle twice.
#[must_use]
pub struct ToggledGuard<T: GuardToggle> {
    /// Whether `new()` actually entered. **Latched at construction.**
    entered: bool,
    _toggle: PhantomData<T>,
}

impl<T: GuardToggle> ToggledGuard<T> {
    /// Enter (if compiled in and enabled) until the returned guard drops.
    #[inline]
    pub fn new() -> Self {
        let entered = T::COMPILED_IN && T::enabled();
        if entered {
            T::enter();
        }
        Self { entered, _toggle: PhantomData }
    }

    /// Take the guard only if `cond` — i.e. this call really is going to reach the
    /// state the guard exists for.
    ///
    /// Used where that work spans a whole function rather than sitting in one `match`
    /// arm (`sys_write`'s per-chunk loop, `sys_lseek`'s `update_fd` closure), so the
    /// arm-local placement used elsewhere isn't available.
    #[inline]
    pub fn new_if(cond: bool) -> Option<Self> {
        cond.then(Self::new)
    }
}

impl<T: GuardToggle> Default for ToggledGuard<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: GuardToggle> Drop for ToggledGuard<T> {
    #[inline]
    fn drop(&mut self) {
        // Latched in `new()` — deliberately NOT a fresh `T::enabled()` read.
        if self.entered {
            T::exit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    thread_local! {
        static ENTERS: Cell<u32> = const { Cell::new(0) };
        static EXITS: Cell<u32> = const { Cell::new(0) };
        static TOGGLE: Cell<bool> = const { Cell::new(true) };
    }

    fn reset(toggle: bool) {
        ENTERS.with(|c| c.set(0));
        EXITS.with(|c| c.set(0));
        TOGGLE.with(|c| c.set(toggle));
    }

    enum On {}
    impl GuardToggle for On {
        const COMPILED_IN: bool = true;
        fn enabled() -> bool {
            TOGGLE.with(Cell::get)
        }
        fn enter() {
            ENTERS.with(|c| c.set(c.get() + 1));
        }
        fn exit() {
            EXITS.with(|c| c.set(c.get() + 1));
        }
    }

    enum Off {}
    impl GuardToggle for Off {
        const COMPILED_IN: bool = false;
        fn enabled() -> bool {
            panic!("a compiled-out guard must never consult its toggle");
        }
        fn enter() {
            panic!("a compiled-out guard must never enter");
        }
        fn exit() {
            panic!("a compiled-out guard must never exit");
        }
    }

    #[test]
    fn enabled_guard_enters_and_exits_once() {
        reset(true);
        {
            let _g = ToggledGuard::<On>::new();
            assert_eq!(ENTERS.with(Cell::get), 1);
            assert_eq!(EXITS.with(Cell::get), 0);
        }
        assert_eq!(EXITS.with(Cell::get), 1);
    }

    #[test]
    fn disabled_toggle_neither_enters_nor_exits() {
        reset(false);
        drop(ToggledGuard::<On>::new());
        assert_eq!(ENTERS.with(Cell::get), 0);
        assert_eq!(EXITS.with(Cell::get), 0, "exit must pair with enter, not with the toggle");
    }

    /// The rule the whole module exists for: a toggle flipped ON→OFF while a guard is
    /// live must still produce the matching exit. A guard that re-read the toggle in
    /// `drop` would skip it and leave the BKL released for the rest of the syscall.
    #[test]
    fn toggle_flip_mid_guard_still_exits() {
        reset(true);
        {
            let _g = ToggledGuard::<On>::new();
            TOGGLE.with(|c| c.set(false));
        }
        assert_eq!(ENTERS.with(Cell::get), 1);
        assert_eq!(EXITS.with(Cell::get), 1);
    }

    /// And the reverse flip, OFF→ON, must not conjure an exit for an entry that never
    /// happened — the same unbalance in the other direction.
    #[test]
    fn reverse_flip_mid_guard_does_not_exit() {
        reset(false);
        {
            let _g = ToggledGuard::<On>::new();
            TOGGLE.with(|c| c.set(true));
        }
        assert_eq!(ENTERS.with(Cell::get), 0);
        assert_eq!(EXITS.with(Cell::get), 0);
    }

    /// `COMPILED_IN = false` short-circuits before the toggle: `Off`'s methods all
    /// panic, so reaching any of them fails this test.
    #[test]
    fn compiled_out_guard_is_inert() {
        drop(ToggledGuard::<Off>::new());
        drop(ToggledGuard::<Off>::new_if(true));
    }

    #[test]
    fn new_if_false_yields_no_guard() {
        reset(true);
        assert!(ToggledGuard::<On>::new_if(false).is_none());
        assert_eq!(ENTERS.with(Cell::get), 0);
        assert!(ToggledGuard::<On>::new_if(true).is_some());
        assert_eq!(ENTERS.with(Cell::get), 1);
        assert_eq!(EXITS.with(Cell::get), 1, "the temporary Some(guard) drops at the semicolon");
    }
}
