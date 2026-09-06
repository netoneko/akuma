//! Leak-hunt counters for the user-frame map.
//!
//! Off by default: with `leak-instr` disabled every hook below compiles to
//! nothing and the statics do not exist. On, they answer one question —
//! *where did the entries go?* — by splitting the live-entry residual into the
//! four routes an entry can leave by.
//!
//! These moved out of `akuma-mmu`'s much larger `instr` module with the map
//! they count. The address-space *lifecycle* counters (`AS_NEW`, the drop
//! bracket, the stuck-drop ring) stayed there, because they count the walker's
//! events; these count this crate's. `akuma_mmu::uf_flow_stats` forwards here
//! so the existing reader is unchanged.
//!
//! `uf_insert`/`uf_removed` fire from inside [`crate::FrameLedger`]'s own
//! methods; `uf_freed_now`/`uf_drop_remainder`/`uf_silent` fire from teardown,
//! which lives with the walker — which is why they are `pub` rather than
//! crate-private.

#[cfg(feature = "leak-instr")]
use core::sync::atomic::{AtomicUsize, Ordering};

/// Entries currently held by a `user_frames` map that still exists: +1 per new
/// key, -1 per key removed, -len when a map is freed or dropped.
#[cfg(feature = "leak-instr")]
static UF_LIVE_ENTRIES: AtomicUsize = AtomicUsize::new(0);
/// Components of that residual, so a short run names the escape route.
#[cfg(feature = "leak-instr")]
static UF_INSERTS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "leak-instr")]
static UF_REMOVE_ONE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "leak-instr")]
static UF_FREED_NOW: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "leak-instr")]
static UF_DROP_REMAINDER: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "leak-instr")]
static UF_SILENT: AtomicUsize = AtomicUsize::new(0);

/// A new key entered a `user_frames` map.
#[cfg(feature = "leak-instr")]
pub fn uf_insert() {
    UF_LIVE_ENTRIES.fetch_add(1, Ordering::Relaxed);
    UF_INSERTS.fetch_add(1, Ordering::Relaxed);
}

/// A key left by its count reaching zero.
#[cfg(feature = "leak-instr")]
pub fn uf_removed() {
    UF_LIVE_ENTRIES.fetch_sub(1, Ordering::Relaxed);
    UF_REMOVE_ONE.fetch_add(1, Ordering::Relaxed);
}

/// `n` keys left because teardown freed them immediately.
#[cfg(feature = "leak-instr")]
pub fn uf_freed_now(n: usize) {
    UF_LIVE_ENTRIES.fetch_sub(n, Ordering::Relaxed);
    UF_FREED_NOW.fetch_add(n, Ordering::Relaxed);
}

/// `n` keys left with the struct they were in.
#[cfg(feature = "leak-instr")]
pub fn uf_drop_remainder(n: usize) {
    if n > 0 {
        UF_LIVE_ENTRIES.fetch_sub(n, Ordering::Relaxed);
        UF_DROP_REMAINDER.fetch_add(n, Ordering::Relaxed);
    }
}

/// `n` keys vanished without being freed or deferred — the route that means a
/// real leak.
#[cfg(feature = "leak-instr")]
pub fn uf_silent(n: usize) {
    if n > 0 {
        UF_LIVE_ENTRIES.fetch_sub(n, Ordering::Relaxed);
        UF_SILENT.fetch_add(n, Ordering::Relaxed);
    }
}

/// `(inserts, removals, freed at teardown, dropped with the struct, silently
/// dropped)`.
#[cfg(feature = "leak-instr")]
#[must_use]
pub fn uf_flow_stats() -> (usize, usize, usize, usize, usize) {
    (
        UF_INSERTS.load(Ordering::Relaxed),
        UF_REMOVE_ONE.load(Ordering::Relaxed),
        UF_FREED_NOW.load(Ordering::Relaxed),
        UF_DROP_REMAINDER.load(Ordering::Relaxed),
        UF_SILENT.load(Ordering::Relaxed),
    )
}

/// Entries held by `user_frames` maps that still exist.
#[cfg(feature = "leak-instr")]
#[must_use]
pub fn uf_live_entries() -> usize {
    UF_LIVE_ENTRIES.load(Ordering::Relaxed)
}

// `leak-instr` off: every hook compiles to nothing.
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
pub fn uf_insert() {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
pub fn uf_removed() {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
pub fn uf_freed_now(_n: usize) {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
pub fn uf_drop_remainder(_n: usize) {}
#[cfg(not(feature = "leak-instr"))]
#[inline(always)]
pub fn uf_silent(_n: usize) {}
#[cfg(not(feature = "leak-instr"))]
#[must_use]
pub fn uf_flow_stats() -> (usize, usize, usize, usize, usize) {
    (0, 0, 0, 0, 0)
}
#[cfg(not(feature = "leak-instr"))]
#[must_use]
pub fn uf_live_entries() -> usize {
    0
}
