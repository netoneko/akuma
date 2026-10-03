//! The shared writable page table: the kernel half of `akuma-fpcache-rw`.
//!
//! Every process that maps page `p` of a file `MAP_SHARED|PROT_WRITE` maps **one
//! frame** for it, writable and never copy-on-write-marked, for as long as any
//! of them maps it. That is what makes one process's store visible to another
//! without a flush — the coherence SQLite's WAL index (`-shm`) depends on and
//! the private-frames design before it could not give
//! (`docs/reference/subsystems/amd64-shared-write-mmap.md`).
//!
//! The decisions — when an entry exists, who wins a fill race, when it goes,
//! where a `write(2)` lands in a page — are the crate's, host-tested. Here is
//! the mechanism: the lock, the PMM's share count, and the physmap copies.
//!
//! # Lock order
//!
//! `TABLE` is a leaf **below** the address-space lock and **above** the PMM's
//! `COW_REFCOUNTS` (taken inside it by `lookup_and_ref`/`publish`, the same
//! nesting `akuma-fpcache` established) and the PMM free path. Nothing here
//! takes an address-space, region or filesystem lock, and IRQs are masked for
//! every hold, for the reason `akuma-fpcache`'s `PAGES` masks them.
//!
//! # Allocation
//!
//! The table is a `BTreeMap`: inserting a page may allocate a node, on the
//! fault path, infallibly. That is the cost `akuma-fpcache` already pays on the
//! same path, and the alternative — a fixed array — would cap how much of a
//! file a database may map shared. Bounded by residency: an entry exists only
//! while a frame for it is mapped, so the table never holds more entries than
//! there are mapped shared-writable pages. Nothing else here allocates: the
//! write-through and zeroing copy straight into frames under the hold.

use core::sync::atomic::{AtomicUsize, Ordering};

use akuma_fpcache_rw::{Generations, Key, Publish, Refs, Table, write_span, zero_span};
use akuma_primitives::irq::with_irqs_disabled;
use spinning_top::Spinlock;

use crate::phys::phys_ptr;

static TABLE: Spinlock<Table> = Spinlock::new(Table::new());

/// `TABLE`'s length, mirrored under the hold so [`active`] can be asked
/// without taking it — it is asked once per `write(2)` on the whole system.
static LEN: AtomicUsize = AtomicUsize::new(0);

/// Fills between their file read and their publish. A write must be observed
/// while one is in flight even if the table is empty — see `Generations`.
static FILLS: AtomicUsize = AtomicUsize::new(0);

static GENS: Generations = Generations::new();

/// Pages mapped from an existing entry (no read, no frame).
pub static HITS: AtomicUsize = AtomicUsize::new(0);
/// Pages filled from the file and published.
pub static MISSES: AtomicUsize = AtomicUsize::new(0);
/// Entries removed because their last mapping went.
pub static REAPS: AtomicUsize = AtomicUsize::new(0);
/// File writes copied into mapped frames.
pub static WRITE_THROUGHS: AtomicUsize = AtomicUsize::new(0);
/// Fills re-read because a write raced them.
pub static FILL_RETRIES: AtomicUsize = AtomicUsize::new(0);

/// The PMM's share count, for the crate.
struct PmmRefs;

impl Refs for PmmRefs {
    fn inc(&self, pa: usize) {
        akuma_pmm::cow_ref_inc(pa);
    }
    fn get(&self, pa: usize) -> u16 {
        akuma_pmm::cow_ref_get(pa)
    }
}

fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    with_irqs_disabled(|| {
        let mut t = TABLE.lock();
        let r = f(&mut t);
        LEN.store(t.len(), Ordering::SeqCst);
        r
    })
}

/// The frame for `key` with a reference taken for the caller's mapping, which
/// the caller must either install or release (`free_page`).
pub fn lookup_and_ref(key: Key) -> Option<usize> {
    let pa = with_irqs_disabled(|| TABLE.lock().lookup_and_ref(key, &PmmRefs))?;
    HITS.fetch_add(1, Ordering::Relaxed);
    Some(pa)
}

/// Start a fill of `inode`: returns the generation [`publish`] compares.
///
/// Counted **before** the generation is read, so a writer that finds no fill
/// in flight and skips the write-through is ordered before this fill's file
/// read (all `SeqCst`), and the read sees its bytes.
pub fn fill_begin(inode: u32) -> u64 {
    FILLS.fetch_add(1, Ordering::SeqCst);
    GENS.read(inode)
}

/// End a fill begun by [`fill_begin`], whatever became of it.
pub fn fill_end() {
    FILLS.fetch_sub(1, Ordering::SeqCst);
}

/// What became of a fill.
pub enum Fill {
    /// `pa` is the page now; the table holds its own reference.
    Inserted,
    /// A peer's frame is the page; a reference for the caller is taken on it,
    /// and the caller's own frame is still entirely the caller's to free.
    Existing(usize),
    /// A write to this inode landed since [`fill_begin`]: read the page again.
    Stale,
    /// The key cannot be shared; map the frame privately.
    Refused,
}

/// Publish a filled frame — unless a write raced the fill (see
/// `akuma_fpcache_rw::Generations`), checked under the same hold the writer's
/// copy takes. `generation` is [`fill_begin`]'s answer; `None` publishes
/// without the check, for a fill that has retried its bound
/// (`mm::shared_write_page` says what that leaves open).
pub fn publish(key: Key, pa: usize, generation: Option<u64>) -> Fill {
    let r = with_table(|t| {
        if generation.is_some_and(|g| GENS.read(key.inode) != g) {
            // A write raced this fill, so `pa` may predate it. An entry a peer
            // published meanwhile is current all the same: either it was in
            // the table when the writer copied (and received the copy), or its
            // own publish saw this same bump and it would not be there. Adopt
            // it if it exists; otherwise read again.
            return match t.lookup_and_ref(key, &PmmRefs) {
                Some(existing) => Fill::Existing(existing),
                None => Fill::Stale,
            };
        }
        match t.publish(key, pa, &PmmRefs) {
            Publish::Inserted => Fill::Inserted,
            Publish::Existing(e) => Fill::Existing(e),
            Publish::Refused => Fill::Refused,
        }
    });
    match r {
        Fill::Inserted => {
            if MISSES.fetch_add(1, Ordering::Relaxed) == 0 {
                crate::serial::puts("[fpcache-rw] first shared writable file page published\n");
            }
        }
        Fill::Stale => {
            FILL_RETRIES.fetch_add(1, Ordering::Relaxed);
        }
        Fill::Existing(_) => {
            HITS.fetch_add(1, Ordering::Relaxed);
        }
        Fill::Refused => {}
    }
    r
}

/// A mapping of `pa` as page `key` just went away (or never landed): drop the
/// entry if nothing maps the frame any more, releasing the table's reference.
///
/// The caller must have written back anything the file should keep first.
/// Safe to call speculatively: a stale or premature call changes nothing.
pub fn reap(key: Key, pa: usize) {
    if with_table(|t| t.reap(key, pa, &PmmRefs)) {
        REAPS.fetch_add(1, Ordering::Relaxed);
        akuma_pmm::free_page(pa, 0);
    }
}

/// `akuma_vfs_glue::MappedFileHooks::active`.
pub fn active() -> bool {
    FILLS.load(Ordering::SeqCst) > 0 || LEN.load(Ordering::SeqCst) > 0
}

/// `akuma_vfs_glue::MappedFileHooks::wrote`: copy a file write into every
/// mapped frame it overlaps, so a mapper sees it as Linux's page cache would.
///
/// **A flush is a write too**, and its source *is* the frame: `munmap`'s
/// write-back hands `write_at` a slice of the physmap. Copying that onto itself
/// would not be a no-op under concurrency — a peer's store landing between the
/// copy's load and its store would be undone — so a span whose source is the
/// destination is skipped.
pub fn wrote(mount_id: u32, inode: u32, offset: usize, data: &[u8]) {
    GENS.bump(inode);
    let mut copied = false;
    with_table(|t| {
        t.for_each_in(mount_id, inode, offset, offset.saturating_add(data.len()), |page_off, pa| {
            let Some(s) = write_span(page_off, offset, data.len()) else {
                return;
            };
            let dst = phys_ptr::<u8>(pa as u64).wrapping_add(s.frame_off);
            let src = data[s.src_off..s.src_off + s.len].as_ptr();
            if core::ptr::eq(dst.cast_const(), src) {
                return;
            }
            // SAFETY: `pa` is a live frame — the table holds a reference to it
            // for as long as the entry exists, and the entry cannot go while
            // this hold is taken. `s` lies inside one page (`write_span`) and
            // inside `data`. `copy`, not `copy_nonoverlapping`: `data` may be a
            // physmap slice of another frame, never this one (checked above),
            // but nothing here proves the general case.
            unsafe { core::ptr::copy(src, dst, s.len) };
            copied = true;
        });
    });
    if copied {
        WRITE_THROUGHS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `akuma_vfs_glue::MappedFileHooks::zeroed`: bytes `[from, to)` of the file
/// read zero now (truncate, hole punch) — and so must the mapped frames.
pub fn zeroed(mount_id: u32, inode: u32, from: usize, to: usize) {
    GENS.bump(inode);
    with_table(|t| {
        t.for_each_in(mount_id, inode, from, to, |page_off, pa| {
            let Some((a, b)) = zero_span(page_off, from, to) else {
                return;
            };
            // SAFETY: as in `wrote`; `[a, b)` lies inside the page.
            unsafe { core::ptr::write_bytes(phys_ptr::<u8>(pa as u64).wrapping_add(a), 0, b - a) };
        });
    });
}

/// ext2's inode-freed hook: `akuma-fpcache`'s invalidation, plus dropping any
/// entry of this table a missed reap left behind. A mapping pins its inode, so
/// by the time the number is freed nothing can map these frames.
pub fn inode_freed(inode: u32) {
    akuma_fpcache::invalidate_inode(inode);
    if LEN.load(Ordering::SeqCst) == 0 {
        return;
    }
    with_table(|t| {
        t.detach_inode(inode, |pa| akuma_pmm::free_page(pa, 0));
    });
}

/// Register the VFS observers. Called once from `fs::init_vfs`.
pub fn init() {
    akuma_vfs_glue::set_mapped_file_hooks(akuma_vfs_glue::MappedFileHooks {
        active,
        wrote,
        zeroed,
    });
}

/// One line for `dmesg`-style diagnostics, written into the caller's buffer.
pub fn stats_line(w: &mut dyn core::fmt::Write) {
    let _ = writeln!(
        w,
        "[FPCACHE-RW] entries={} hits={} misses={} reaps={} write_through={} fill_retries={}",
        LEN.load(Ordering::Relaxed),
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        REAPS.load(Ordering::Relaxed),
        WRITE_THROUGHS.load(Ordering::Relaxed),
        FILL_RETRIES.load(Ordering::Relaxed),
    );
}
