//! A page that is reserved but not yet there, written into its own leaf entry.
//!
//! Neither CPU reads anything of a leaf whose bit 0 is clear (`P` on amd64, the
//! valid bit of an AArch64 descriptor), so the other 63 bits are the software's:
//! Linux keeps swap entries there. A tokio kernel has no region table and keeps
//! its **demand-paging record** there instead: what the page will be when it is
//! first touched. The layout is the same on both architectures:
//!
//! | bits | field |
//! |---|---|
//! | 0 | [`VALID`] (`P` / the descriptor's valid bit), always clear |
//! | 1 | [`TAG`]: this entry is a lazy page, not an empty slot |
//! | 2 | writable once present |
//! | 3 | executable once present |
//! | 4 | file-backed |
//! | 5 | `PROT_NONE`: never filled, every access faults |
//! | 6..=37 | inode number (32 bits) |
//! | 38 | `MAP_SHARED`: one frame per file page for every mapper, not a private copy |
//! | 39..=63 | file page index (25 bits: offsets below 128 GiB) |
//!
//! An anonymous lazy page is the tag and the permission bits; its inode and
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
//! Every walker tests the valid bit before anything else (`akuma-pagetable`'s lookup, the
//! clone and free plans of `akuma-user-tree`, a kernel's teardown), so a lazy
//! entry is "not mapped" to all of them. That is right for translation and for
//! freeing — a lazy entry names no frame — and wrong in exactly two places,
//! which the kernel that writes these handles itself: fork must copy it (the
//! clone plan skips a non-present leaf), and unmapping must clear it (`unmap`
//! returns `None` for it and leaves it standing).

/// Bit 0 of a leaf: `P` on amd64, the valid bit on AArch64. Clear in every
/// entry this module writes; [`decode`] refuses any entry with it set.
pub const VALID: u64 = 1 << 0;

/// Bit 1 of an invalid leaf: a lazy page. (Bit 1 means something else in a
/// valid entry on either architecture, which is irrelevant here because
/// [`VALID`] is clear in every entry this module writes.)
pub const TAG: u64 = 1 << 1;
const WRITE: u64 = 1 << 2;
const EXEC: u64 = 1 << 3;
const FILE: u64 = 1 << 4;
const NONE: u64 = 1 << 5;
const INODE_SHIFT: u32 = 6;
const SHARED: u64 = 1 << 38;
const INDEX_SHIFT: u32 = 39;
/// The largest file page index an entry can name (`2^25 - 1`).
pub const MAX_INDEX: u64 = (1 << (64 - INDEX_SHIFT)) - 1;

/// What a lazy page becomes on its first touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lazy {
    /// Map the page writable.
    pub write: bool,
    /// Map the page executable.
    pub exec: bool,
    /// `PROT_NONE`: the page is reserved and inaccessible. It is never filled,
    /// so every access faults; `write` and `exec` are ignored while it is set.
    pub none: bool,
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
    /// A writable `MAP_SHARED` page: every mapper maps the one frame for this page of the file, and stores
    /// reach the file. A private page is filled into a frame of its own.
    pub shared: bool,
}

impl Lazy {
    /// A zero-fill page.
    #[must_use]
    pub const fn anon(write: bool, exec: bool) -> Self {
        Self { write, exec, none: false, file: None }
    }

    /// The entry, or `None` when the file page index does not fit (an offset at
    /// or past 128 GiB) - the caller maps that page another way.
    #[must_use]
    pub const fn encode(self) -> Option<u64> {
        let mut v = TAG;
        if self.write {
            v |= WRITE;
        }
        if self.exec {
            v |= EXEC;
        }
        if self.none {
            v |= NONE;
        }
        if let Some(f) = self.file {
            if f.index > MAX_INDEX {
                return None;
            }
            v |= FILE | ((f.inode as u64) << INODE_SHIFT) | (f.index << INDEX_SHIFT);
            if f.shared {
                v |= SHARED;
            }
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
                    Some(Self { file: Some(FilePage { index, ..f }), ..self })
                }
                _ => None,
            },
        }
    }

    /// The same page with new permissions (`mprotect` on a page not yet touched);
    /// `none` makes it inaccessible.
    #[must_use]
    pub const fn with_prot(self, write: bool, exec: bool, none: bool) -> Self {
        Self { write, exec, none, ..self }
    }
}

/// The lazy page `entry` records, or `None` for anything else: a present leaf,
/// an empty slot, or a not-present entry without [`TAG`].
#[must_use]
pub const fn decode(entry: u64) -> Option<Lazy> {
    if entry & VALID != 0 || entry & TAG == 0 {
        return None;
    }
    let file = if entry & FILE != 0 {
        Some(FilePage { inode: (entry >> INODE_SHIFT) as u32, index: entry >> INDEX_SHIFT, shared: entry & SHARED != 0 })
    } else {
        None
    };
    Some(Lazy { write: entry & WRITE != 0, exec: entry & EXEC != 0, none: entry & NONE != 0, file })
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
            let l = Lazy { write: true, exec: false, none: false, file: Some(FilePage { inode, index, shared: false }) };
            assert_eq!(decode(l.encode().unwrap()), Some(l));
        }
    }

    #[test]
    fn an_index_past_the_field_does_not_encode() {
        let l = Lazy { write: false, exec: false, none: false, file: Some(FilePage { inode: 7, index: MAX_INDEX + 1, shared: false }) };
        assert_eq!(l.encode(), None);
        let at_limit = Lazy { file: Some(FilePage { inode: 7, index: MAX_INDEX, shared: false }), ..l };
        assert_eq!(at_limit.offset_by(1), None);
        assert_eq!(at_limit.offset_by(0), Some(at_limit));
    }

    #[test]
    fn no_lazy_entry_is_present_and_none_is_empty() {
        let l = Lazy { write: true, exec: true, none: true, file: Some(FilePage { inode: u32::MAX, index: MAX_INDEX, shared: false }) };
        for e in [Lazy::anon(false, false).encode().unwrap(), l.encode().unwrap()] {
            assert_eq!(e & VALID, 0, "a lazy entry must fault");
            assert_ne!(e, 0, "a lazy entry must not read as an empty slot");
        }
    }

    #[test]
    fn present_and_untagged_entries_are_not_lazy() {
        assert!(!is_lazy(0));
        // A present leaf whose low bits happen to read as the tag (valid + bit 1).
        assert!(!is_lazy(VALID | TAG | 0x1000));
        // Not present, but no tag: whatever else wrote it, it is not ours.
        assert!(!is_lazy(FILE | (5 << INODE_SHIFT)));
    }

    #[test]
    fn offset_by_advances_a_file_page_and_leaves_anonymous_alone() {
        let f = Lazy { write: false, exec: true, none: false, file: Some(FilePage { inode: 9, index: 3, shared: false }) };
        assert_eq!(f.offset_by(4).unwrap().file.unwrap().index, 7);
        assert_eq!(Lazy::anon(true, false).offset_by(1 << 40), Some(Lazy::anon(true, false)));
    }

    #[test]
    fn with_prot_keeps_the_backing() {
        let f = Lazy { write: true, exec: false, none: false, file: Some(FilePage { inode: 9, index: 3, shared: false }) };
        let r = f.with_prot(false, true, false);
        assert_eq!((r.write, r.exec, r.none, r.file), (false, true, false, f.file));
        let n = r.with_prot(false, false, true);
        assert_eq!(decode(n.encode().unwrap()), Some(n));
        assert!(!decode(n.with_prot(true, false, false).encode().unwrap()).unwrap().none);
    }

    #[test]
    fn shared_round_trips_and_advances_with_the_index() {
        let f = Lazy { write: true, exec: false, none: false, file: Some(FilePage { inode: u32::MAX, index: MAX_INDEX, shared: true }) };
        assert_eq!(decode(f.encode().unwrap()), Some(f));
        let g = Lazy { file: Some(FilePage { inode: 9, index: 3, shared: true }), ..f };
        let next = g.offset_by(5).unwrap().file.unwrap();
        assert_eq!((next.index, next.shared, next.inode), (8, true, 9));
        // The bit is the file's: a private page of the same file does not read as shared.
        let p = Lazy { file: Some(FilePage { shared: false, ..g.file.unwrap() }), ..g };
        assert!(!decode(p.encode().unwrap()).unwrap().file.unwrap().shared);
    }

    #[test]
    fn none_round_trips_with_and_without_a_file() {
        let a = Lazy { none: true, ..Lazy::anon(false, false) };
        assert_eq!(decode(a.encode().unwrap()), Some(a));
        let f = Lazy { none: true, file: Some(FilePage { inode: u32::MAX, index: MAX_INDEX, shared: false }), ..a };
        assert_eq!(decode(f.encode().unwrap()), Some(f));
    }
}
