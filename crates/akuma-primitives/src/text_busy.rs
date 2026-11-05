//! "Text file busy": which inodes are being executed, so the filesystem can refuse to change them (`ETXTBSY`).
//!
//! # Why this exists
//!
//! A process's code can be read from its executable's inode *after* `execve` has returned: a whole-page segment page is a lazy file
//! record, filled from the file on the first touch. Nothing stopped a writer from rewriting that file, so the running program would
//! crash on (or, worse, execute) whatever was written, and a signature checked at exec described bytes that no longer ran. Linux
//! refuses: a file being executed cannot be opened for writing or truncated (`ETXTBSY`). This is that rule, enforced where the
//! writes land (in the filesystem, by inode) rather than at `open`, because this kernel has no open-file object to count writers
//! on, and a descriptor opened for writing *before* the exec must be stopped too, which a check at `open` cannot do.
//!
//! # What an entry is keyed on
//!
//! `(filesystem identity, inode)` (`Filesystem::identity`, unique per mounted instance, never `0`). Inode numbers repeat across mounts, so
//! a mark on inode 12 of one ext2 must not make inode 12 of another read as busy; keyed on the number alone it did, and a test run found
//! it (two separately mounted test filesystems refusing each other's writes). A filesystem with identity `0` has no marks.
//!
//! # Shape
//!
//! [`TextPin`] is held by an executing process (taken before its image is read, released when it exits or execs again).
//! [`is_busy`] is asked by the filesystem inside its write and truncate paths. A `TextPin` also holds an
//! [`InodePin`](crate::InodePin), so unlinking or renaming over a running binary (which stays allowed, as on Linux) defers the free
//! of its blocks until the last process using it is gone.
//!
//! **Exact, and it fails closed.** The table is small and has no overflow mode: when it is full, [`TextPin::try_new`] returns
//! `None` and the caller refuses the `execve`, so a lookup never answers "busy" for an inode that is not and never answers "not
//! busy" for one that is. (The [`InodePin`](crate::InodePin) table's conservative-on-overflow answer would turn a lost pin into
//! spurious `ETXTBSY` on unrelated files.) Lookups are lock-free; the rare insert and remove take a spinlock with preemption off.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::{InodePin, PreemptGuard};

/// Distinct inodes that can be executing at once. A `-j4` build runs tens of distinct binaries; past this an `execve` fails.
pub const SLOTS: usize = 256;

/// `(fs << 32) | inode` in each slot; `0` is empty (neither identity `0` nor inode `0` is ever marked).
static KEYS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// How many executing processes hold each slot's inode.
static COUNTS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
/// Held for the whole of an insert or remove.
static LOCK: AtomicBool = AtomicBool::new(false);

/// The lock, held with preemption off; the guard is kept only for its `Drop`.
struct Locked {
    _preempt: PreemptGuard,
}

fn lock() -> Locked {
    let guard = PreemptGuard::new();
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    Locked { _preempt: guard }
}

impl Drop for Locked {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

const fn key(fs: u32, inode: u32) -> u64 {
    ((fs as u64) << 32) | inode as u64
}

/// Is inode `inode` of filesystem `fs` being executed by some process? Lock-free; the filesystem asks this on every data write and
/// truncate.
#[must_use]
pub fn is_busy(fs: u32, inode: u32) -> bool {
    fs != 0 && inode != 0 && KEYS.iter().any(|s| s.load(Ordering::Acquire) == key(fs, inode))
}

/// How many inodes are being executed (diagnostics and tests).
#[must_use]
pub fn busy_count() -> usize {
    KEYS.iter().filter(|s| s.load(Ordering::Acquire) != 0).count()
}

fn acquire(k: u64) -> bool {
    let _l = lock();
    let mut free = None;
    for (i, s) in KEYS.iter().enumerate() {
        let cur = s.load(Ordering::Relaxed);
        if cur == k {
            COUNTS[i].fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if cur == 0 && free.is_none() {
            free = Some(i);
        }
    }
    let Some(i) = free else {
        return false;
    };
    COUNTS[i].store(1, Ordering::Relaxed);
    // Published last: a lookup that sees the inode sees a non-zero count behind it.
    KEYS[i].store(k, Ordering::Release);
    true
}

fn release(k: u64) {
    let _l = lock();
    for (i, s) in KEYS.iter().enumerate() {
        if s.load(Ordering::Relaxed) == k {
            if COUNTS[i].fetch_sub(1, Ordering::Relaxed) == 1 {
                s.store(0, Ordering::Release);
            }
            return;
        }
    }
}

/// A reference that marks `inode` as being executed. Clone to share (a fork child runs the same image), drop to release.
///
/// Not `Copy`: the count is kept by `Clone` and `Drop`, so every path that copies or destroys a process record stays balanced
/// without a call site doing arithmetic.
#[derive(Debug)]
pub struct TextPin {
    fs: u32,
    inode: u32,
    /// Keeps the inode's blocks alive past an unlink or a rename over it, as a running binary's must be.
    _keep: InodePin,
}

impl TextPin {
    /// Mark inode `inode` of filesystem `fs` as being executed. `fs == 0` or `inode == 0` yields an inert pin (nothing to mark).
    /// `None` when the table is full: refuse the `execve`.
    #[must_use]
    pub fn try_new(fs: u32, inode: u32) -> Option<Self> {
        if fs == 0 || inode == 0 {
            return Some(Self::none());
        }
        acquire(key(fs, inode)).then(|| Self { fs, inode, _keep: InodePin::new(inode) })
    }

    /// An inert pin: a process with no executable on a filesystem (a kernel thread, a built-in).
    #[must_use]
    pub const fn none() -> Self {
        Self { fs: 0, inode: 0, _keep: InodePin::none() }
    }

    /// The inode this pin marks, `0` for an inert one.
    #[must_use]
    pub const fn inode(&self) -> u32 {
        self.inode
    }
}

impl Clone for TextPin {
    fn clone(&self) -> Self {
        if self.inode != 0 {
            // The entry exists (this pin holds it), so this only raises its count and cannot fail.
            let _ = acquire(key(self.fs, self.inode));
        }
        Self { fs: self.fs, inode: self.inode, _keep: self._keep.clone() }
    }
}

impl Drop for TextPin {
    fn drop(&mut self) {
        if self.inode != 0 {
            release(key(self.fs, self.inode));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};
    use std::vec::Vec;

    use super::*;

    // The table is a process-wide static and `cargo test` runs threads. Each test owns a range of inode numbers no other test uses, and
    // they take this lock because one of them fills every slot: run beside it, any other test's pin would be refused.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn a_pin_marks_the_inode_busy_until_the_last_one_drops() {
        let _s = serial();
        let a = TextPin::try_new(7, 900_001).unwrap();
        assert!(is_busy(7, 900_001));
        let b = a.clone();
        drop(a);
        assert!(is_busy(7, 900_001), "the clone still holds it");
        drop(b);
        assert!(!is_busy(7, 900_001));
    }

    #[test]
    fn other_inodes_and_inode_zero_are_never_busy() {
        let _s = serial();
        let _p = TextPin::try_new(7, 900_010).unwrap();
        assert!(!is_busy(7, 900_011));
        assert!(!is_busy(7, 0));
        assert_eq!(TextPin::try_new(7, 0).unwrap().inode(), 0, "inode 0 is the inert pin");
        assert_eq!(TextPin::try_new(0, 5).unwrap().inode(), 0, "identity 0 is the inert pin");
        assert!(!is_busy(0, 900_010) && !is_busy(7, 0));
    }

    #[test]
    fn the_same_inode_number_on_another_filesystem_is_not_busy() {
        let _s = serial();
        let _p = TextPin::try_new(31, 900_030).unwrap();
        assert!(is_busy(31, 900_030));
        assert!(!is_busy(32, 900_030), "inode numbers repeat across mounts; only the pinned instance's file is busy");
    }

    #[test]
    fn a_text_pin_also_keeps_the_inode_pinned_for_the_filesystem() {
        let _s = serial();
        let p = TextPin::try_new(7, 900_020).unwrap();
        assert!(crate::inode_pin::is_pinned(900_020));
        drop(p);
        assert!(!crate::inode_pin::is_pinned(900_020));
    }

    #[test]
    fn a_full_table_refuses_the_pin_and_never_lies_about_the_rest() {
        let _s = serial();
        // Fill what is left of the table with distinct inodes, then one more must be refused.
        let mut held = Vec::new();
        let mut n = 910_000;
        while let Some(p) = TextPin::try_new(9, n) {
            held.push(p);
            n += 1;
            assert!(held.len() <= SLOTS, "more pins than slots");
        }
        assert!(held.len() <= SLOTS);
        assert!(TextPin::try_new(9, n).is_none(), "full: refused, not conservative");
        // Every pin that was granted is busy; the refused inode is not.
        assert!(held.iter().all(|p| is_busy(9, p.inode())));
        assert!(!is_busy(9, n));
        // Releasing one makes room again.
        let first = held.remove(0);
        let freed = first.inode();
        drop(first);
        assert!(!is_busy(9, freed));
        assert!(TextPin::try_new(9, n).is_some());
    }

    #[test]
    fn concurrent_pins_and_drops_balance() {
        let _s = serial();
        let ts: Vec<_> = (0..4)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..500 {
                        let ino = 920_000 + t * 10 + (i % 5);
                        let p = TextPin::try_new(11, ino).unwrap();
                        assert!(is_busy(11, ino));
                        let q = p.clone();
                        drop(p);
                        assert!(is_busy(11, ino));
                        drop(q);
                    }
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        for t in 0..4 {
            for k in 0..5 {
                assert!(!is_busy(11, 920_000 + t * 10 + k), "every pin released");
            }
        }
    }
}
