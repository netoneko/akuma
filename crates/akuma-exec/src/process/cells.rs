//! The `Process` fields that change after the process is published.
//!
//! A published `Process` is reached only through `&Process`: the table hands
//! out `&'static Process` borrows that outlive any IRQ mask and live on other
//! cores (`akuma-slot-table`'s `active_ref`, `ref_if_current`). So a field that
//! is written after publication cannot be a plain field written through a
//! `&mut Process` — that `&mut` would alias every one of those borrows, which
//! is undefined behaviour however the write is serialized. `table::with_process`
//! handed out exactly that `&mut` until 2026-11-16.
//!
//! These are the fields it was used to write, each as a cell written through
//! `&self`: the scalars as atomics, the channel `Arc` inside a lock taken with
//! IRQs masked (it is read from paths that run masked, so an unmasked hold
//! could deadlock a core against its own interrupt), and the two path strings
//! (`cwd`, `root`) inside a lock.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use spinning_top::Spinlock;

use super::Pid;
use super::channel::ProcessChannel;
use crate::runtime::with_irqs_disabled;

/// A pid that can be re-pointed (`parent_pid`, on reparenting to init).
#[derive(Debug)]
pub struct PidCell(AtomicU32);

impl PidCell {
    #[must_use]
    pub const fn new(pid: Pid) -> Self {
        Self(AtomicU32::new(pid))
    }
    #[inline]
    #[must_use]
    pub fn get(&self) -> Pid {
        self.0.load(Ordering::Acquire)
    }
    #[inline]
    pub fn set(&self, pid: Pid) {
        self.0.store(pid, Ordering::Release);
    }
}

/// `u64::MAX` is `None`; every pid and every thread slot index is far below it.
const NONE: u64 = u64::MAX;

/// An optional pid (`delegate_pid`, `grabbed_by`).
#[derive(Debug)]
pub struct OptPidCell(AtomicU64);

impl OptPidCell {
    #[must_use]
    pub const fn new(v: Option<Pid>) -> Self {
        Self(AtomicU64::new(match v {
            Some(p) => p as u64,
            None => NONE,
        }))
    }
    #[inline]
    #[must_use]
    pub fn get(&self) -> Option<Pid> {
        let raw = self.0.load(Ordering::Acquire);
        (raw != NONE).then_some(raw as Pid)
    }
    #[inline]
    pub fn set(&self, v: Option<Pid>) {
        self.0.store(v.map_or(NONE, u64::from), Ordering::Release);
    }
}

/// An optional scheduler slot index (`thread_id`), stored as the thread's incarnation.
///
/// The cell keeps a packed [`akuma_threading::ThreadRef`] (slot and generation in one word, so a reader never sees a new
/// slot with an old generation). [`get`](Self::get) is the slot as it always was; [`get_live`](Self::get_live) is the
/// slot only if it still hosts the thread that was recorded. A stale tid acting on a recycled slot is the largest bug
/// class in the tree by incident count (`TYPE_SYSTEM_SAFETY_REPORT.md` §5: `KTG_STALE_TID_EXIT_STAMP_J4_HANG`,
/// `STALE_THREAD_SLOT_KILL`, `AKUMA_STALE_TID_ZOMBIE_SLOT`); anything that *acts on* the thread should use `get_live`.
/// A check-then-act on the generation is still a race: the window is one load, not the lifetime of the process.
#[derive(Debug)]
pub struct OptTidCell(AtomicU64);

impl OptTidCell {
    /// A cell for a tid that may not be live yet (or at all, in a synthetic test process): the recorded generation is
    /// `0`, which is what a slot that never recycles carries. Prefer [`new_live`](Self::new_live) when `v` is live now.
    #[must_use]
    pub const fn new(v: Option<usize>) -> Self {
        Self(AtomicU64::new(match v {
            Some(t) => t as u64,
            None => NONE,
        }))
    }
    /// A cell for a tid that is live right now: records its current incarnation.
    #[must_use]
    pub fn new_live(v: Option<usize>) -> Self {
        let cell = Self::new(None);
        cell.set(v);
        cell
    }
    #[inline]
    #[must_use]
    pub fn get(&self) -> Option<usize> {
        let raw = self.0.load(Ordering::Acquire);
        (raw != NONE).then(|| akuma_threading::ThreadRef::from_raw(raw).tid())
    }
    /// The slot, only if it still hosts the incarnation recorded when the cell was set. `None` for a thread that
    /// exited and whose slot was recycled, which [`get`](Self::get) would still name.
    #[inline]
    #[must_use]
    pub fn get_live(&self) -> Option<usize> {
        let raw = self.0.load(Ordering::Acquire);
        if raw == NONE {
            return None;
        }
        let r = akuma_threading::ThreadRef::from_raw(raw);
        r.is_current().then(|| r.tid())
    }
    /// Record `v` as live now (its current generation).
    #[inline]
    pub fn set(&self, v: Option<usize>) {
        self.0.store(v.map_or(NONE, |t| akuma_threading::ThreadRef::of_live_tid(t).raw()), Ordering::Release);
    }
}

/// The process's I/O channel, which `reattach` re-points.
pub struct ChannelCell(Spinlock<Option<Arc<ProcessChannel>>>);

impl ChannelCell {
    #[must_use]
    pub const fn new(ch: Option<Arc<ProcessChannel>>) -> Self {
        Self(Spinlock::new(ch))
    }
    /// A clone of the current channel. Cloning an `Arc` does not allocate.
    #[must_use]
    pub fn get(&self) -> Option<Arc<ProcessChannel>> {
        with_irqs_disabled(|| self.0.lock().clone())
    }
    /// Whether there is a channel at all.
    #[must_use]
    pub fn is_some(&self) -> bool {
        with_irqs_disabled(|| self.0.lock().is_some())
    }
    /// Install `ch`, dropping the channel it replaced.
    pub fn set(&self, ch: Option<Arc<ProcessChannel>>) {
        drop(self.replace(ch));
    }
    /// Install `ch`, returning the channel it replaced. The caller drops the
    /// old one outside any mask of its own.
    pub fn replace(&self, ch: Option<Arc<ProcessChannel>>) -> Option<Arc<ProcessChannel>> {
        with_irqs_disabled(|| core::mem::replace(&mut *self.0.lock(), ch))
    }
}

/// A `u64` written after publication (`robust_list_head`).
#[derive(Debug)]
pub struct U64Cell(AtomicU64);

impl U64Cell {
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(AtomicU64::new(v))
    }
    #[inline]
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
    #[inline]
    pub fn set(&self, v: u64) {
        self.0.store(v, Ordering::Release);
    }
}

/// A `usize` written after publication (`robust_list_len`).
#[derive(Debug)]
pub struct UsizeCell(AtomicUsize);

impl UsizeCell {
    #[must_use]
    pub const fn new(v: usize) -> Self {
        Self(AtomicUsize::new(v))
    }
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
    #[inline]
    pub fn set(&self, v: usize) {
        self.0.store(v, Ordering::Release);
    }
}

/// A path string that `chdir`/`chroot` replace (`cwd`, `root`).
///
/// `get` clones, so it allocates: call it outside an IRQ-masked region, as the
/// plain `p.cwd.clone()` it replaces had to be. `set` only swaps under the lock
/// and drops the old string after releasing it.
pub struct StrCell(Spinlock<String>);

impl StrCell {
    #[must_use]
    pub const fn new(s: String) -> Self {
        Self(Spinlock::new(s))
    }
    #[must_use]
    pub fn get(&self) -> String {
        self.0.lock().clone()
    }
    /// Run `f` on the string without cloning it. `f` must not block.
    pub fn with<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        f(&self.0.lock())
    }
    pub fn set(&self, s: String) {
        let old = core::mem::replace(&mut *self.0.lock(), s);
        drop(old);
    }
}
