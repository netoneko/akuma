//! Writable `MAP_SHARED` file pages: the kernel half of `akuma-fpcache-rw`.
//!
//! Every process that maps page `p` of a file `MAP_SHARED|PROT_WRITE` maps one
//! frame for it, writable and never copy-on-write-marked, for as long as any of
//! them maps it. One process's store is then visible to another without a
//! flush, which SQLite's WAL index (`-shm`) depends on. Until 2026-11-03 this
//! kernel gave each mapper private frames filled at `mmap` and copied them on
//! fork, so two processes on one database diverged.
//!
//! The decisions - when an entry exists, who wins a fill race, when it goes,
//! where a `write(2)` lands in a page - are the crate's, host-tested. This is
//! amd64's `shmpages.rs` on this kernel's frames: the lock, the PMM's share
//! count, and the physmap copies (`akuma_mmu::copy_to_phys`, bounds-checked
//! against PMM RAM, so nothing here is `unsafe`).
//!
//! # Lock order
//!
//! `TABLE` is a leaf: taken with IRQs masked, under the kernel lock, above the
//! PMM's `COW_REFCOUNTS` (taken inside it by `lookup_and_ref`/`publish`, the
//! nesting `akuma-fpcache` established). Nothing here takes a filesystem lock;
//! the fill's file read happens between [`fill_begin`] and [`publish`], outside
//! the hold.
//!
//! # Allocation
//!
//! A `BTreeMap` node per mapped page, on the fault path, as `akuma-fpcache`
//! pays. Bounded by residency: an entry exists only while a frame for it is
//! mapped somewhere.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use akuma_fpcache_rw::{Generations, Key, Publish, Refs, Table, write_span, zero_span};
use akuma_primitives::irq::with_irqs_disabled;
use spinning_top::Spinlock;

static TABLE: Spinlock<Table> = Spinlock::new(Table::new());

/// `TABLE`'s length, mirrored so [`active`] is a load, not a lock: it is asked
/// once per `write(2)` on the whole system.
static LEN: AtomicUsize = AtomicUsize::new(0);

/// Fills between their file read and their publish. A write must be observed
/// while one is in flight even if the table is empty - see `Generations`.
static FILLS: AtomicUsize = AtomicUsize::new(0);

static GENS: Generations = Generations::new();

/// The root filesystem's mount id in glue's table: the identity half of every
/// key, and what glue's write hook passes, so a write to another mount (`/proc`)
/// matches no entry. 0 until [`init`]; a 0 key is never shareable.
static ROOT_MOUNT: AtomicU32 = AtomicU32::new(0);

// Counters for a debugger or a future `/proc` line; only the first publish is
// announced (on the console, as amd64 does).

/// Pages mapped from an existing entry (no read, no frame).
static HITS: AtomicUsize = AtomicUsize::new(0);
/// Pages filled from the file and published.
static MISSES: AtomicUsize = AtomicUsize::new(0);
/// Entries removed because their last mapping went.
static REAPS: AtomicUsize = AtomicUsize::new(0);

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

/// The key for page `index` of `inode` on the root filesystem.
pub fn key(inode: u32, index: u64) -> Key {
    Key::new(ROOT_MOUNT.load(Ordering::Relaxed), inode, index as usize * 4096)
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
/// Counted before the generation is read, so a writer that finds no fill in
/// flight and skips the write-through is ordered before this fill's file read
/// (all `SeqCst`), and the read sees its bytes.
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
    /// The frame is the page now; the table holds its own reference.
    Inserted,
    /// A peer's frame is the page; a reference for the caller is taken on it,
    /// and the caller's own frame is still entirely the caller's to free.
    Existing(usize),
    /// A write to this inode landed since [`fill_begin`]: read the page again.
    Stale,
    /// The key cannot be shared; map the frame privately.
    Refused,
}

/// Publish a filled frame, unless a write raced the fill (checked under the
/// hold the writer's copy takes). `generation` is [`fill_begin`]'s answer;
/// `None` publishes without the check, after the caller's retry bound.
pub fn publish(key: Key, pa: usize, generation: Option<u64>) -> Fill {
    let r = with_table(|t| {
        if generation.is_some_and(|g| GENS.read(key.inode) != g) {
            // A peer's entry published meanwhile is current all the same: it
            // was in the table when the writer copied, or its own publish saw
            // this bump and it would not be there.
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
                akuma_primitives::safe_print!(80, "[fpcache-rw] first shared writable file page published\n");
            }
        }
        Fill::Existing(_) => {
            HITS.fetch_add(1, Ordering::Relaxed);
        }
        Fill::Stale | Fill::Refused => {}
    }
    r
}

/// A mapping of `pa` as page `key` just went away (or never landed): drop the
/// entry if nothing maps the frame any more, releasing the table's reference.
/// The caller must have written back anything the file should keep first. A
/// stale or premature call changes nothing.
pub fn reap(key: Key, pa: usize) {
    if with_table(|t| t.reap(key, pa, &PmmRefs)) {
        REAPS.fetch_add(1, Ordering::Relaxed);
        akuma_pmm::free_page(pa, 0);
    }
}

/// Drop every entry nobody maps any more, freeing its frame. This kernel's
/// teardown frees an address space from its page table alone, after the
/// records naming each page's key are gone, so it sweeps rather than reaping
/// page by page (`release_pages`, `free_user_root`). Every path that drops a
/// shared page's last mapping writes the page back first.
pub fn sweep() {
    if LEN.load(Ordering::SeqCst) == 0 {
        return;
    }
    let n = with_table(|t| t.reap_unmapped(&PmmRefs, |pa| akuma_pmm::free_page(pa, 0)));
    REAPS.fetch_add(n, Ordering::Relaxed);
}

/// `MappedFileHooks::active`.
fn active() -> bool {
    FILLS.load(Ordering::SeqCst) > 0 || LEN.load(Ordering::SeqCst) > 0
}

/// `MappedFileHooks::wrote`: copy a file write into every mapped frame it
/// overlaps, as Linux's page cache would show it. A span whose source is the
/// frame itself (a write-back handing `write_at` the physmap) is skipped:
/// copying it onto itself could undo a peer's store landing in between.
fn wrote(mount_id: u32, inode: u32, offset: usize, data: &[u8]) {
    GENS.bump(inode);
    with_table(|t| {
        t.for_each_in(mount_id, inode, offset, offset.saturating_add(data.len()), |page_off, pa| {
            let Some(s) = write_span(page_off, offset, data.len()) else {
                return;
            };
            let src = &data[s.src_off..s.src_off + s.len];
            if core::ptr::eq(akuma_primitives::addr::phys_to_virt(pa + s.frame_off).cast_const(), src.as_ptr()) {
                return;
            }
            // The table holds a reference to `pa` while the entry exists, and
            // the entry cannot go during this hold.
            let _ = akuma_mmu::copy_to_phys(pa + s.frame_off, src);
        });
    });
}

/// `MappedFileHooks::zeroed`: bytes `[from, to)` of the file read zero now
/// (truncate, hole punch), and so must the mapped frames.
fn zeroed(mount_id: u32, inode: u32, from: usize, to: usize) {
    static ZEROS: [u8; 4096] = [0; 4096];
    GENS.bump(inode);
    with_table(|t| {
        t.for_each_in(mount_id, inode, from, to, |page_off, pa| {
            if let Some((a, b)) = zero_span(page_off, from, to) {
                let _ = akuma_mmu::copy_to_phys(pa + a, &ZEROS[..b - a]);
            }
        });
    });
}

/// ext2's inode-freed hook: the read-only cache's invalidation, plus dropping
/// any entry a missed reap left behind. A mapping pins its inode, so by the
/// time the number is freed nothing maps these frames.
#[cfg(feature = "disk")]
pub fn inode_freed(inode: u32) {
    akuma_fpcache::invalidate_inode(inode);
    if LEN.load(Ordering::SeqCst) == 0 {
        return;
    }
    with_table(|t| {
        t.detach_inode(inode, |pa| akuma_pmm::free_page(pa, 0));
    });
}

/// Record the root mount's id and register glue's write observers. Called once
/// from `mount_root`, after glue's mount table knows the root.
pub fn init() {
    if let Some((id, _)) = akuma_vfs_glue::resolve_file_id("/") {
        ROOT_MOUNT.store(id, Ordering::Relaxed);
    }
    akuma_vfs_glue::set_mapped_file_hooks(akuma_vfs_glue::MappedFileHooks { active, wrote, zeroed });
}
