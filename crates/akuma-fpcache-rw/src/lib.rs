//! One physical frame per `(mount, inode, page)` for **writable `MAP_SHARED`**
//! file mappings — the table, the race rules and the span arithmetic.
//!
//! # Why this exists
//!
//! The amd64 kernel served writable `MAP_SHARED` file mappings (2026-09-22) by
//! demand-paged fills plus write-back on `munmap`/`msync`, with **no page
//! cache**: every mapper held its own frames, so two processes mapping one file
//! did not see each other's stores until a flush landed. That was a documented
//! limit until SQLite met it. Its WAL index (the `-shm` file) is exactly such a
//! mapping, shared live by every connection in every process; with a private
//! copy per process the copies diverged and goose's multi-process `sessions.db`
//! came back `database disk image is malformed`
//! (`docs/archive/AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md` § 15).
//!
//! The fix is the one Linux's page cache gives for free: all mappers of a page
//! map **one frame**, writable, never copy-on-write-marked.
//!
//! # Why a sibling of `akuma-fpcache` and not more of it
//!
//! `akuma-fpcache` shares frames for **read-only** mappings, and three of its
//! rules are wrong for writable ones:
//!
//! - **It evicts and invalidates.** Dropping a read-only entry costs a re-read.
//!   Dropping a writable entry while a mapper still holds the frame leaves that
//!   mapper on an orphan and the next mapper on a fresh copy from disk — the
//!   incoherence this crate exists to remove. Here an entry lives **exactly** as
//!   long as some address space maps its frame ([`Table::reap`]), and is never
//!   evicted for capacity.
//! - **A lost insert race keeps the loser private.** Harmless for read-only
//!   bytes; for a writable page it is two copies again. [`Table::publish`] hands
//!   the loser the winner's frame instead.
//! - **`write(2)` invalidates.** A writable page may hold stores the file has not
//!   seen yet, so dropping it loses them; the file write is instead copied
//!   *into* the mapped frame ([`write_span`]), which is what a unified page cache
//!   means by `write(2)` being visible through `mmap`.
//!
//! # The reference count
//!
//! Frames are counted by the PMM's copy-on-write table, as `akuma-fpcache`'s are.
//! Its convention: an **absent** entry (count 0) is one implicit owner, and the
//! first increment creates the entry at 2. So with one table entry and `m`
//! mapping address spaces the count is `1 + m`, and **`<= 1` means "nobody maps
//! it"** — the one test [`Table::reap`] makes. The kernel supplies the count
//! through [`Refs`] so the rules can be tested here without a PMM.
//!
//! # What stays in the kernel
//!
//! The lock (IRQ-masked, a leaf below the address-space lock), the real
//! refcount, allocating and filling frames, copying bytes through the physmap,
//! and every page-table edit. Nothing in this crate touches memory it does not
//! own, which is why it can be `#![forbid(unsafe_code)]`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

/// The page size every offset here is a multiple of.
pub const PAGE_SIZE: usize = 4096;

/// Which page of which file.
///
/// **Inode-major field order**, so the derived `Ord` makes every page of one
/// inode a contiguous range — what lets [`Table::for_each_in`] and
/// [`Table::detach_inode`] be a range walk rather than a sweep, the same reason
/// `akuma-fpcache` orders its key this way.
///
/// The mount id is part of the identity for the reason `akuma-fpcache` gives
/// (its finding F-1): an inode number means nothing without the filesystem that
/// issued it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub inode: u32,
    pub mount_id: u32,
    /// Page-aligned byte offset in the file.
    pub offset: usize,
}

impl Key {
    #[must_use]
    pub const fn new(mount_id: u32, inode: u32, offset: usize) -> Self {
        Self { inode, mount_id, offset }
    }

    /// Whether this key can name a shared page at all.
    ///
    /// Inode 0 is `InodePin`'s "no inode" sentinel and mount id 0 is never
    /// issued; an unaligned offset cannot be a page. A mapping whose identity
    /// fails this keeps private frames, exactly as before this crate existed.
    #[must_use]
    pub const fn is_shareable(&self) -> bool {
        self.inode != 0 && self.mount_id != 0 && self.offset.is_multiple_of(PAGE_SIZE)
    }
}

/// The frame share count, as the kernel keeps it. See the crate docs for the
/// convention ("absent is one owner, the first increment makes it 2").
pub trait Refs {
    fn inc(&self, pa: usize);
    fn get(&self, pa: usize) -> u16;
}

/// What [`Table::publish`] did with a freshly filled frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publish {
    /// The frame is now the page. The table took its own reference; the
    /// caller's frame keeps its implicit one, which becomes the mapping's.
    Inserted,
    /// A peer published this page first. The caller must **discard its own
    /// frame** (it holds no table reference) and map this one instead — a
    /// reference for the caller's mapping has already been taken on it.
    ///
    /// The difference from `akuma-fpcache`, where the loser keeps its frame
    /// private: for a writable page that would be two copies, which is the bug.
    Existing(usize),
    /// The key cannot be shared ([`Key::is_shareable`]). Nothing was taken.
    Refused,
}

/// The table: one frame per page that some address space maps.
pub struct Table {
    map: BTreeMap<Key, usize>,
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Table {
    #[must_use]
    pub const fn new() -> Self {
        Self { map: BTreeMap::new() }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The frame for `key`, with a reference taken for the caller's mapping.
    ///
    /// The reference is taken **under the caller's lock, while the entry is
    /// present** — the rule `akuma-fpcache` learned the hard way (its W1): taking
    /// it after the lock drops races a reap that frees the frame, and the late
    /// increment resurrects a freed page as file content.
    pub fn lookup_and_ref(&self, key: Key, refs: &impl Refs) -> Option<usize> {
        let pa = *self.map.get(&key)?;
        refs.inc(pa);
        Some(pa)
    }

    /// Publish `pa`, freshly filled from the file, as the page for `key` —
    /// or adopt the frame a peer published first. See [`Publish`].
    pub fn publish(&mut self, key: Key, pa: usize, refs: &impl Refs) -> Publish {
        if !key.is_shareable() {
            return Publish::Refused;
        }
        if let Some(&existing) = self.map.get(&key) {
            refs.inc(existing);
            return Publish::Existing(existing);
        }
        self.map.insert(key, pa);
        // The table's own reference, taken as the entry becomes visible and
        // only when it does (`akuma-fpcache`'s W2: taking it on the lost-race
        // path too leaked one count per race).
        refs.inc(pa);
        Publish::Inserted
    }

    /// Drop the entry for `key` if it still names `pa` **and nobody maps it**.
    ///
    /// `true` means the entry is gone and the caller must release the table's
    /// reference (which frees the frame). Called after a mapping lets go of a
    /// frame; a stale call — the entry was replaced, or a peer still maps it, or
    /// a peer mapped it again in between — is a no-op, which is what makes it
    /// safe to call from every path that might have been the last.
    ///
    /// **The caller must already have flushed** anything the frame holds that
    /// the file should keep: after this, the only copy of the page is the file.
    pub fn reap(&mut self, key: Key, pa: usize, refs: &impl Refs) -> bool {
        if self.map.get(&key) != Some(&pa) || refs.get(pa) > 1 {
            return false;
        }
        self.map.remove(&key);
        true
    }

    /// Visit every cached page of `(mount_id, inode)` that overlaps the byte
    /// range `[from, to)`, as `(page_offset, pa)`, ascending.
    pub fn for_each_in(&self, mount_id: u32, inode: u32, from: usize, to: usize, mut f: impl FnMut(usize, usize)) {
        if to <= from {
            return;
        }
        let lo = Key::new(mount_id, inode, from & !(PAGE_SIZE - 1));
        let hi = Key::new(mount_id, inode, usize::MAX);
        for (k, &pa) in self.map.range(lo..=hi) {
            if k.offset >= to {
                break;
            }
            f(k.offset, pa);
        }
    }

    /// Remove every entry of `inode`, **on every mount**, handing each frame to
    /// `f` so the caller can release the table's reference.
    ///
    /// For the filesystem's inode-freed hook, which cannot say which mount it
    /// fires for (`akuma-fpcache::invalidate_inode` has the argument). By then no
    /// mapping can remain — a mapping pins its inode — so this only collects
    /// what a missed reap left behind. Returns how many entries went.
    pub fn detach_inode(&mut self, inode: u32, mut f: impl FnMut(usize)) -> usize {
        let lo = Key::new(0, inode, 0);
        let hi = Key::new(u32::MAX, inode, usize::MAX);
        let mut n = 0;
        while let Some((&k, &pa)) = self.map.range(lo..=hi).next() {
            self.map.remove(&k);
            f(pa);
            n += 1;
        }
        n
    }
}

/// Where a write of `write_len` bytes at file offset `write_off` lands in the
/// page that starts at `page_off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Byte offset inside the frame.
    pub frame_off: usize,
    /// Byte offset inside the written data.
    pub src_off: usize,
    pub len: usize,
}

/// The part of a file write that lands in one cached page, or `None` if none
/// does. Saturating throughout: every input is ultimately a ring-3 register.
#[must_use]
pub fn write_span(page_off: usize, write_off: usize, write_len: usize) -> Option<Span> {
    let write_end = write_off.saturating_add(write_len);
    let page_end = page_off.saturating_add(PAGE_SIZE);
    let start = write_off.max(page_off);
    let end = write_end.min(page_end);
    if start >= end {
        return None;
    }
    Some(Span { frame_off: start - page_off, src_off: start - write_off, len: end - start })
}

/// The frame-relative byte range `[a, b)` of the page at `page_off` that falls
/// inside the file range `[from, to)` being zeroed.
///
/// Zeroed by `ftruncate` (`to` is `usize::MAX`: everything past the new end
/// reads zero if the file grows back, which is what Linux's truncate does to a
/// mapped page) or by a hole punch.
#[must_use]
pub fn zero_span(page_off: usize, from: usize, to: usize) -> Option<(usize, usize)> {
    write_span(page_off, from, to.saturating_sub(from)).map(|s| (s.frame_off, s.frame_off + s.len))
}

/// Buckets in [`Generations`]. A power of two; inodes hash by their low bits.
pub const GENERATION_BUCKETS: usize = 256;

/// Per-inode-bucket write generations: the fix for the **fill race**.
///
/// A page is filled by reading the file and *then* publishing the frame. A
/// `write(2)` that lands in the file between the read and the publish finds no
/// entry to copy into, and the frame published afterwards holds the bytes from
/// before it — a mapped page silently older than the file. The writer cannot
/// see the fill; the fill can see the writer: every write-through [`bump`]s the
/// inode's bucket before it takes the table lock, and a fill that [`read`]s the
/// bucket before its file read publishes only if the bucket is unchanged, under
/// the same lock. Otherwise it reads again. A bucket shared with an unrelated
/// busy file costs a spurious re-read, never a wrong page.
///
/// [`bump`]: Generations::bump
/// [`read`]: Generations::read
pub struct Generations {
    buckets: [AtomicU64; GENERATION_BUCKETS],
}

impl Default for Generations {
    fn default() -> Self {
        Self::new()
    }
}

impl Generations {
    #[must_use]
    pub const fn new() -> Self {
        Self { buckets: [const { AtomicU64::new(0) }; GENERATION_BUCKETS] }
    }

    const fn slot(inode: u32) -> usize {
        inode as usize & (GENERATION_BUCKETS - 1)
    }

    /// A write to `inode` is about to be copied into the table.
    pub fn bump(&self, inode: u32) {
        self.buckets[Self::slot(inode)].fetch_add(1, Ordering::SeqCst);
    }

    /// The bucket's generation, to compare at publish time.
    #[must_use]
    pub fn read(&self, inode: u32) -> u64 {
        self.buckets[Self::slot(inode)].load(Ordering::SeqCst)
    }
}

/// Should a present page be mapped **copy-on-write-marked** (read-only until a
/// write breaks the sharing)?
///
/// `amd64/src/mm.rs::pte_prot_for`'s rule — a writable page whose frame is
/// shared (`refs > 0`) is marked, so the first write copies — plus the exception
/// it was missing: a page that is shared **by identity** (`MAP_SHARED`,
/// anonymous or file) must never be marked, because "copy on write" is then
/// precisely "stop sharing", the opposite of the flag. Before this, an
/// `mprotect(PROT_READ)` then `mprotect(PROT_READ|PROT_WRITE)` over a forked
/// `MAP_SHARED|MAP_ANONYMOUS` page re-marked it, and the next write split the
/// parent from the child.
#[must_use]
pub const fn marks_cow(shared_by_identity: bool, writable: bool, refs: u16) -> bool {
    !shared_by_identity && writable && refs > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    extern crate std;

    /// The PMM's convention, modelled: absent = one owner, first inc = 2.
    #[derive(Default)]
    struct FakeRefs(RefCell<BTreeMap<usize, u16>>);

    impl FakeRefs {
        /// `pmm::free_page`: drop one reference; `true` when it was the last.
        fn dec(&self, pa: usize) -> bool {
            let mut m = self.0.borrow_mut();
            match m.get_mut(&pa) {
                Some(c) => {
                    *c -= 1;
                    if *c == 0 {
                        m.remove(&pa);
                        true
                    } else {
                        false
                    }
                }
                None => true,
            }
        }
    }

    impl Refs for FakeRefs {
        fn inc(&self, pa: usize) {
            let mut m = self.0.borrow_mut();
            match m.get_mut(&pa) {
                Some(c) => *c += 1,
                None => {
                    m.insert(pa, 2);
                }
            }
        }
        fn get(&self, pa: usize) -> u16 {
            self.0.borrow().get(&pa).copied().unwrap_or(0)
        }
    }

    const K: Key = Key::new(1, 12, 0x8000);

    #[test]
    fn first_mapper_publishes_and_the_count_is_mapping_plus_table() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        assert_eq!(t.publish(K, 0x1000, &r), Publish::Inserted);
        assert_eq!(r.get(0x1000), 2, "the filler's mapping + the table");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn second_mapper_hits_the_same_frame() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        t.publish(K, 0x1000, &r);
        assert_eq!(t.lookup_and_ref(K, &r), Some(0x1000));
        assert_eq!(r.get(0x1000), 3);
        assert_eq!(t.lookup_and_ref(Key::new(1, 12, 0x9000), &r), None);
    }

    /// The race `akuma-fpcache` lets the loser lose privately. Here the loser
    /// must end up on the winner's frame, with a reference to it, and must not
    /// have put a reference on its own.
    #[test]
    fn a_lost_publish_race_adopts_the_winner() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        t.publish(K, 0x1000, &r);
        assert_eq!(t.publish(K, 0x2000, &r), Publish::Existing(0x1000));
        assert_eq!(r.get(0x1000), 3, "winner's mapping + table + loser's mapping");
        assert_eq!(r.get(0x2000), 0, "the loser's frame was never referenced: it frees outright");
    }

    #[test]
    fn unshareable_keys_are_refused_without_a_reference() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        for k in [Key::new(0, 12, 0), Key::new(1, 0, 0), Key::new(1, 12, 100)] {
            assert_eq!(t.publish(k, 0x1000, &r), Publish::Refused);
        }
        assert!(t.is_empty());
        assert_eq!(r.get(0x1000), 0);
    }

    /// The whole lifetime: two mappers, each unmaps, and the entry goes exactly
    /// when the second one does — not before (the first unmapper must not take
    /// the page from under the second) and not never (a leak per page).
    #[test]
    fn the_entry_lives_exactly_as_long_as_a_mapping() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        t.publish(K, 0x1000, &r);
        t.lookup_and_ref(K, &r);
        // First unmapper: drop its reference, then try to reap.
        assert!(!r.dec(0x1000));
        assert!(!t.reap(K, 0x1000, &r), "the other mapper still maps it");
        assert_eq!(t.len(), 1);
        // Second unmapper.
        assert!(!r.dec(0x1000), "the table's reference keeps it alive");
        assert!(t.reap(K, 0x1000, &r));
        assert!(t.is_empty());
        assert!(r.dec(0x1000), "releasing the table's reference frees the frame");
    }

    /// A reap that arrives after a peer mapped the page again must not take it.
    #[test]
    fn a_reap_racing_a_new_mapper_keeps_the_entry() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        t.publish(K, 0x1000, &r);
        r.dec(0x1000); // the only mapper left: count 1
        t.lookup_and_ref(K, &r); // a new mapper arrives before the reap: 2
        assert!(!t.reap(K, 0x1000, &r));
        assert_eq!(t.lookup_and_ref(K, &r), Some(0x1000));
    }

    /// A reap naming a frame the entry no longer holds is a no-op.
    #[test]
    fn a_stale_reap_is_a_no_op() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        t.publish(K, 0x1000, &r);
        r.dec(0x1000);
        assert!(!t.reap(K, 0x2000, &r));
        assert!(!t.reap(Key::new(2, 12, 0x8000), 0x1000, &r), "same inode, other mount");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn for_each_in_visits_only_the_overlapping_pages_of_one_file() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        for (i, off) in [0usize, 0x1000, 0x2000, 0x5000].iter().enumerate() {
            t.publish(Key::new(1, 12, *off), 0x10_000 * (i + 1), &r);
        }
        t.publish(Key::new(2, 12, 0x1000), 0xa0_000, &r); // same inode, other mount
        t.publish(Key::new(1, 13, 0x1000), 0xb0_000, &r); // other inode
        let mut seen = Vec::new();
        t.for_each_in(1, 12, 0x1800, 0x2001, |off, pa| seen.push((off, pa)));
        assert_eq!(seen, vec![(0x1000, 0x20_000), (0x2000, 0x30_000)]);
        seen.clear();
        t.for_each_in(1, 12, 0x3000, usize::MAX, |off, pa| seen.push((off, pa)));
        assert_eq!(seen, vec![(0x5000, 0x40_000)]);
        seen.clear();
        t.for_each_in(1, 12, 0x3000, 0x3000, |off, pa| seen.push((off, pa)));
        assert!(seen.is_empty(), "an empty range visits nothing");
    }

    #[test]
    fn detach_inode_takes_every_mount_and_nothing_else() {
        let (mut t, r) = (Table::new(), FakeRefs::default());
        t.publish(Key::new(1, 12, 0), 0x1000, &r);
        t.publish(Key::new(2, 12, 0x1000), 0x2000, &r);
        t.publish(Key::new(1, 13, 0), 0x3000, &r);
        let mut freed = Vec::new();
        assert_eq!(t.detach_inode(12, |pa| freed.push(pa)), 2);
        freed.sort_unstable();
        assert_eq!(freed, vec![0x1000, 0x2000]);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn write_span_clips_to_the_page() {
        // Entirely inside.
        assert_eq!(write_span(0x1000, 0x1010, 4), Some(Span { frame_off: 0x10, src_off: 0, len: 4 }));
        // Starts before the page.
        assert_eq!(write_span(0x1000, 0x0ff0, 0x20), Some(Span { frame_off: 0, src_off: 0x10, len: 0x10 }));
        // Runs past the page.
        assert_eq!(write_span(0x1000, 0x1ff8, 0x10), Some(Span { frame_off: 0xff8, src_off: 0, len: 8 }));
        // Covers it and more.
        assert_eq!(write_span(0x1000, 0, 0x3000), Some(Span { frame_off: 0, src_off: 0x1000, len: 0x1000 }));
        // Misses on either side, and the zero-length write.
        assert_eq!(write_span(0x1000, 0, 0x1000), None);
        assert_eq!(write_span(0x1000, 0x2000, 4), None);
        assert_eq!(write_span(0x1000, 0x1100, 0), None);
        // A ring-3-sized length cannot wrap into "misses".
        assert_eq!(write_span(0x1000, 0x1800, usize::MAX), Some(Span { frame_off: 0x800, src_off: 0, len: 0x800 }));
    }

    /// SQLite extends its `-shm` by writing one zero byte at the last byte of
    /// each new page. That byte lands at the page's end, and only there.
    #[test]
    fn sqlite_shm_extension_byte_lands_on_the_last_byte() {
        assert_eq!(write_span(0x7000, 0x7fff, 1), Some(Span { frame_off: 0xfff, src_off: 0, len: 1 }));
    }

    #[test]
    fn zero_span_for_truncate_and_punch() {
        // Truncate to mid-page: the tail of that page, all of the later ones.
        assert_eq!(zero_span(0x1000, 0x1800, usize::MAX), Some((0x800, 0x1000)));
        assert_eq!(zero_span(0x2000, 0x1800, usize::MAX), Some((0, 0x1000)));
        // Pages wholly before the new end are untouched.
        assert_eq!(zero_span(0x0000, 0x1800, usize::MAX), None);
        // Truncate to a page boundary leaves the page before it alone.
        assert_eq!(zero_span(0x0000, 0x1000, usize::MAX), None);
        // A punched hole.
        assert_eq!(zero_span(0x1000, 0x1100, 0x1200), Some((0x100, 0x200)));
        assert_eq!(zero_span(0x1000, 0x1200, 0x1100), None, "an inverted range is empty");
    }

    #[test]
    fn generations_detect_a_write_between_read_and_publish() {
        let g = Generations::new();
        let before = g.read(12);
        g.bump(12 + GENERATION_BUCKETS as u32); // same bucket: a spurious retry, never a miss
        assert_ne!(g.read(12), before);
        let before = g.read(12);
        g.bump(13);
        assert_eq!(g.read(12), before, "another bucket does not disturb this one");
    }

    #[test]
    fn identity_shared_pages_are_never_cow_marked() {
        assert!(marks_cow(false, true, 2), "a forked private page is marked");
        assert!(!marks_cow(false, true, 0), "an unshared private page is not");
        assert!(!marks_cow(false, false, 2), "a read-only page needs no mark");
        assert!(!marks_cow(true, true, 2), "MAP_SHARED: shared by identity, never marked");
    }
}
