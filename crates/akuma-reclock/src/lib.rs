//! POSIX advisory record locks — the table, and nothing else.
//!
//! # Why this exists
//!
//! `fcntl(F_SETLK)` answered `0` and `F_GETLK` "unlocked" for every caller, so no
//! two processes ever excluded each other. That is invisible until something
//! builds its integrity on the exclusion: **SQLite's WAL index** is a shared
//! memory file whose writers serialise on byte-range locks, and goose runs a
//! main process plus several child processes against one `sessions.db`. With no
//! exclusion two of them wrote the WAL together and the next open said
//! `database disk image is malformed` (2026-10-03, trashcan).
//!
//! # Semantics kept (POSIX, not OFD)
//!
//! - The owner is a **process** (the caller passes an opaque `u64`; glue uses its
//!   fd-table identity, so `CLONE_FILES` threads share it and a `fork` child does
//!   not inherit).
//! - Locks are per file, keyed by an opaque string (glue passes the path, as
//!   `flock` does).
//! - A process's locks never conflict with its own: a new lock **replaces** the
//!   overlapping part of its old ones (upgrade, downgrade, or unlock a sub-range,
//!   splitting a lock in two).
//! - `release_file` drops everything an owner holds on a file — POSIX's rule that
//!   closing *any* descriptor for a file releases the process's locks on it.
//!
//! Ranges are half-open `[start, end)`; `end == u64::MAX` is "to end of file"
//! (`l_len == 0`). The table is a `Vec` per file: SQLite holds a handful.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// `F_RDLCK` / `F_WRLCK`. (`F_UNLCK` is an operation, not a stored kind.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
}

/// One held lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock {
    pub owner: u64,
    /// The owner's pid, for `F_GETLK`'s `l_pid`. Opaque to the table.
    pub pid: u32,
    pub kind: Kind,
    pub start: u64,
    /// Exclusive. `u64::MAX` means "to end of file".
    pub end: u64,
}

/// `[start, end)` from `(l_start, l_len)` already resolved against `l_whence`.
/// `len == 0` is to EOF. `None` for a range that is empty after resolution or
/// overflows — `EINVAL` in the caller.
#[must_use]
pub fn range(start: i64, len: i64) -> Option<(u64, u64)> {
    // A negative length locks the `len` bytes *before* `start` (POSIX 2008).
    let (s, e) = if len >= 0 {
        let s = u64::try_from(start).ok()?;
        let e = if len == 0 { u64::MAX } else { s.checked_add(len as u64)? };
        (s, e)
    } else {
        let e = u64::try_from(start).ok()?;
        let s = e.checked_sub(len.unsigned_abs())?;
        (s, e)
    };
    (s < e).then_some((s, e))
}

fn overlaps(a: &Lock, start: u64, end: u64) -> bool {
    a.start < end && start < a.end
}

/// All files' locks.
#[derive(Default)]
pub struct Table {
    files: BTreeMap<String, Vec<Lock>>,
}

impl Table {
    #[must_use]
    pub const fn new() -> Self {
        Self { files: BTreeMap::new() }
    }

    /// The first lock held by someone **other than** `owner` that a `kind` lock
    /// over `[start, end)` would collide with — `F_GETLK`'s answer, and
    /// `F_SETLK`'s refusal. Two read locks never collide.
    #[must_use]
    pub fn conflict(&self, file: &str, owner: u64, kind: Kind, start: u64, end: u64) -> Option<Lock> {
        self.files.get(file)?.iter().copied().find(|l| {
            l.owner != owner
                && overlaps(l, start, end)
                && (kind == Kind::Write || l.kind == Kind::Write)
        })
    }

    /// Take the lock if nothing conflicts; on conflict return the blocker and
    /// change nothing. The caller decides whether to wait (`F_SETLKW`).
    #[allow(clippy::too_many_arguments)]
    pub fn try_lock(
        &mut self,
        file: &str,
        owner: u64,
        pid: u32,
        kind: Kind,
        start: u64,
        end: u64,
    ) -> Result<(), Lock> {
        if let Some(b) = self.conflict(file, owner, kind, start, end) {
            return Err(b);
        }
        self.carve(file, owner, start, end);
        let v = self.files.entry(String::from(file)).or_default();
        v.push(Lock { owner, pid, kind, start, end });
        merge_adjacent(v, owner, kind);
        Ok(())
    }

    /// `F_UNLCK` over `[start, end)`: remove the range from `owner`'s locks,
    /// splitting any that straddle it.
    pub fn unlock(&mut self, file: &str, owner: u64, start: u64, end: u64) {
        self.carve(file, owner, start, end);
        self.prune(file);
    }

    /// Drop every lock `owner` holds on `file` (a close of any descriptor for it).
    pub fn release_file(&mut self, file: &str, owner: u64) {
        if let Some(v) = self.files.get_mut(file) {
            v.retain(|l| l.owner != owner);
        }
        self.prune(file);
    }

    /// Drop everything `owner` holds anywhere (process exit).
    pub fn release_owner(&mut self, owner: u64) {
        self.files.retain(|_, v| {
            v.retain(|l| l.owner != owner);
            !v.is_empty()
        });
    }

    /// Does anyone hold a lock on `file`? For tests and diagnostics.
    #[must_use]
    pub fn is_locked(&self, file: &str) -> bool {
        self.files.get(file).is_some_and(|v| !v.is_empty())
    }

    /// `owner`'s locks on `file`, for tests.
    #[must_use]
    pub fn held(&self, file: &str, owner: u64) -> Vec<Lock> {
        let mut out: Vec<Lock> =
            self.files.get(file).map_or_else(Vec::new, |v| v.iter().copied().filter(|l| l.owner == owner).collect());
        out.sort_by_key(|l| l.start);
        out
    }

    /// Remove `[start, end)` from `owner`'s locks on `file`, splitting.
    fn carve(&mut self, file: &str, owner: u64, start: u64, end: u64) {
        let Some(v) = self.files.get_mut(file) else { return };
        let mut out = Vec::with_capacity(v.len() + 1);
        for l in v.drain(..) {
            if l.owner != owner || !overlaps(&l, start, end) {
                out.push(l);
                continue;
            }
            if l.start < start {
                out.push(Lock { end: start, ..l });
            }
            if end < l.end {
                out.push(Lock { start: end, ..l });
            }
        }
        *v = out;
    }

    fn prune(&mut self, file: &str) {
        if self.files.get(file).is_some_and(Vec::is_empty) {
            self.files.remove(file);
        }
    }
}

/// Coalesce touching/overlapping same-owner same-kind locks so repeated
/// lock/unlock cycles do not grow the list without bound.
fn merge_adjacent(v: &mut Vec<Lock>, owner: u64, kind: Kind) {
    let mut mine: Vec<Lock> = v.iter().copied().filter(|l| l.owner == owner && l.kind == kind).collect();
    if mine.len() < 2 {
        return;
    }
    mine.sort_by_key(|l| l.start);
    let mut merged: Vec<Lock> = Vec::with_capacity(mine.len());
    for l in mine {
        match merged.last_mut() {
            Some(last) if l.start <= last.end => last.end = last.end.max(l.end),
            _ => merged.push(l),
        }
    }
    v.retain(|l| !(l.owner == owner && l.kind == kind));
    v.extend(merged);
}

#[cfg(test)]
mod tests;
