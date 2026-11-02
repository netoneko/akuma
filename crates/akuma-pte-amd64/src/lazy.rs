//! A page that is reserved but not yet there, written into its own leaf entry.
//!
//! The CPU reads nothing of a leaf whose `P` bit is clear, so the other 63 bits
//! are the software's: Linux keeps swap entries there. A kernel with no region
//! table (Taoxichuan's `akuma-tokio-kernel-amd64`) keeps its **demand-paging
//! record** there instead — what the page will be when it is first touched:
//!
//! | bits | field |
//! |---|---|
//! | 0 | `P`, always clear |
//! | 1 | [`TAG`]: this entry is a lazy page, not an empty slot |
//! | 2 | writable once present |
//! | 3 | executable once present |
//! | 4 | file-backed |
//! | 5..=36 | inode number (32 bits) |
//! | 37..=63 | file page index (27 bits: offsets below 512 GiB) |
//!
//! An anonymous lazy page is the tag and the two permission bits; its inode and
//! index fields are zero.
//!
//! # Why the page table and not a region list
//!
//! Every edit a mapping goes through is per page already — `munmap` clips, a
//! `MAP_FIXED` lands inside a reservation, `mprotect` covers part of one,
//! `mremap` moves one — so a record that lives in the page's own entry is
//! clipped, split and moved by the same writes that clip, split and move the
//! page. Fork copies it with the rest of the leaf table. The cost is the page
//! tables a reservation needs before any page of it is touched: 2 KiB of tables
//! per MiB reserved, against 1 MiB of zeroed frames when every page was mapped
//! at `mmap`.
//!
//! # What an entry must never be mistaken for
//!
//! Every walker tests `P` before anything else (`akuma-pagetable`'s lookup, the
//! clone and free plans of `akuma-user-tree`, a kernel's teardown), so a lazy
//! entry is "not mapped" to all of them. That is right for translation and for
//! freeing — a lazy entry names no frame — and wrong in exactly two places,
//! which the kernel that writes these handles itself: fork must copy it (the
//! clone plan skips a non-present leaf), and unmapping must clear it (`unmap`
//! returns `None` for it and leaves it standing).

use crate::P;

/// Bit 1 of a not-present leaf: a lazy page. Bit 1 is `RW` in a present entry,
/// which is irrelevant here because `P` is clear in every entry this module
/// writes, and [`decode`] refuses any entry with `P` set.
pub const TAG: u64 = 1 << 1;
const WRITE: u64 = 1 << 2;
const EXEC: u64 = 1 << 3;
const FILE: u64 = 1 << 4;
const INODE_SHIFT: u32 = 5;
const INDEX_SHIFT: u32 = 37;
/// The largest file page index an entry can name (`2^27 - 1`).
pub const MAX_INDEX: u64 = (1 << (64 - INDEX_SHIFT)) - 1;

/// What a lazy page becomes on its first touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lazy {
    /// Map the page writable.
    pub write: bool,
    /// Map the page executable.
    pub exec: bool,
    /// Fill the page from this file instead of zeroing it.
    pub file: Option<FilePage>,
}

/// A page of a file: the inode and the page index (file offset / 4096).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePage {
    /// The inode number.
    pub inode: u32,
    /// The file offset of the page's first byte, in pages.
    pub index: u64,
}

impl Lazy {
    /// A zero-fill page.
    #[must_use]
    pub const fn anon(write: bool, exec: bool) -> Self {
        Self { write, exec, file: None }
    }

    /// The entry, or `None` when the file page index does not fit (an offset at
    /// or past 512 GiB) - the caller maps that page another way.
    #[must_use]
    pub const fn encode(self) -> Option<u64> {
        let mut v = TAG;
        if self.write {
            v |= WRITE;
        }
        if self.exec {
            v |= EXEC;
        }
        if let Some(f) = self.file {
            if f.index > MAX_INDEX {
                return None;
            }
            v |= FILE | ((f.inode as u64) << INODE_SHIFT) | (f.index << INDEX_SHIFT);
        }
        Some(v)
    }

    /// The same page, `pages` further on: a file page's index advances with it.
    /// `None` past [`MAX_INDEX`].
    #[must_use]
    pub const fn offset_by(self, pages: u64) -> Option<Self> {
        match self.file {
            None => Some(self),
            Some(f) => match f.index.checked_add(pages) {
                Some(index) if index <= MAX_INDEX => {
                    Some(Self { file: Some(FilePage { inode: f.inode, index }), ..self })
                }
                _ => None,
            },
        }
    }

    /// The same page with new permissions (`mprotect` on a page not yet touched).
    #[must_use]
    pub const fn with_prot(self, write: bool, exec: bool) -> Self {
        Self { write, exec, ..self }
    }
}

/// The lazy page `entry` records, or `None` for anything else: a present leaf,
/// an empty slot, or a not-present entry without [`TAG`].
#[must_use]
pub const fn decode(entry: u64) -> Option<Lazy> {
    if entry & P != 0 || entry & TAG == 0 {
        return None;
    }
    let file = if entry & FILE != 0 {
        Some(FilePage { inode: (entry >> INODE_SHIFT) as u32, index: entry >> INDEX_SHIFT })
    } else {
        None
    };
    Some(Lazy { write: entry & WRITE != 0, exec: entry & EXEC != 0, file })
}

/// Is `entry` a lazy page?
#[must_use]
pub const fn is_lazy(entry: u64) -> bool {
    decode(entry).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_round_trips_with_each_permission() {
        for (w, x) in [(false, false), (true, false), (false, true), (true, true)] {
            let l = Lazy::anon(w, x);
            assert_eq!(decode(l.encode().unwrap()), Some(l));
        }
    }

    #[test]
    fn file_round_trips_at_the_field_limits() {
        for (inode, index) in [(1, 0), (u32::MAX, MAX_INDEX), (0x1234_5678, 76_000)] {
            let l = Lazy { write: true, exec: false, file: Some(FilePage { inode, index }) };
            assert_eq!(decode(l.encode().unwrap()), Some(l));
        }
    }

    #[test]
    fn an_index_past_the_field_does_not_encode() {
        let l = Lazy { write: false, exec: false, file: Some(FilePage { inode: 7, index: MAX_INDEX + 1 }) };
        assert_eq!(l.encode(), None);
        let at_limit = Lazy { file: Some(FilePage { inode: 7, index: MAX_INDEX }), ..l };
        assert_eq!(at_limit.offset_by(1), None);
        assert_eq!(at_limit.offset_by(0), Some(at_limit));
    }

    #[test]
    fn no_lazy_entry_is_present_and_none_is_empty() {
        let l = Lazy { write: true, exec: true, file: Some(FilePage { inode: u32::MAX, index: MAX_INDEX }) };
        for e in [Lazy::anon(false, false).encode().unwrap(), l.encode().unwrap()] {
            assert_eq!(e & P, 0, "a lazy entry must fault");
            assert_ne!(e, 0, "a lazy entry must not read as an empty slot");
        }
    }

    #[test]
    fn present_and_untagged_entries_are_not_lazy() {
        assert!(!is_lazy(0));
        // A present leaf whose low bits happen to read as the tag (P|RW).
        assert!(!is_lazy(P | TAG | 0x1000));
        // Not present, but no tag: whatever else wrote it, it is not ours.
        assert!(!is_lazy(FILE | (5 << INODE_SHIFT)));
    }

    #[test]
    fn offset_by_advances_a_file_page_and_leaves_anonymous_alone() {
        let f = Lazy { write: false, exec: true, file: Some(FilePage { inode: 9, index: 3 }) };
        assert_eq!(f.offset_by(4).unwrap().file.unwrap().index, 7);
        assert_eq!(Lazy::anon(true, false).offset_by(1 << 40), Some(Lazy::anon(true, false)));
    }

    #[test]
    fn with_prot_keeps_the_backing() {
        let f = Lazy { write: true, exec: false, file: Some(FilePage { inode: 9, index: 3 }) };
        let r = f.with_prot(false, true);
        assert_eq!((r.write, r.exec, r.file), (false, true, f.file));
    }
}
