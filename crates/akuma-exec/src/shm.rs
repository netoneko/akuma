//! Writable `MAP_SHARED` file pages, for both kernels: the kernel half of
//! `akuma-fpcache-rw`.
//!
//! Every process that maps page `p` of a file `MAP_SHARED|PROT_WRITE` maps **one
//! frame** for it, writable and never copy-on-write-marked, for as long as any
//! of them maps it. One process's store is then visible to another without a
//! flush, which SQLite's WAL index (`-shm`) depends on
//! (`docs/reference/subsystems/amd64-shared-write-mmap.md`). The decisions —
//! when an entry exists, who wins a fill race, when it goes, where a `write(2)`
//! lands in a page — are the crate's, host-tested. Here is the mechanism: the
//! lock, the PMM's share count, the physmap copies (`akuma_mmu::copy_to_phys`,
//! bounds-checked, so nothing here is `unsafe`) and the fault-time fill.
//!
//! Moved from amd64's `shmpages.rs` (2026-11-12, step 6 of
//! `proposals/AKUMA_MM_CONVERGENCE.md`); the VFS observers are registered by
//! `akuma_syscalls_glue::mem::init_shared_write`.
//!
//! # Lock order
//!
//! `TABLE` is a leaf **below** the address-space lock and **above** the PMM's
//! `COW_REFCOUNTS` (taken inside it by `lookup_and_ref`/`publish`, the nesting
//! `akuma-fpcache` established) and the PMM free path. Nothing here takes a
//! region or filesystem lock under it, and IRQs are masked for every hold.
//!
//! # Allocation
//!
//! The table is a `BTreeMap`: inserting a page may allocate a node, on the fault
//! path. Bounded by residency — an entry exists only while a frame for it is
//! mapped. The write-through and zeroing copy straight into frames.

use core::sync::atomic::{AtomicUsize, Ordering};

use akuma_fpcache_rw::{Generations, Publish, Refs, Table, write_span, zero_span};
pub use akuma_fpcache_rw::Key;
use akuma_primitives::irq::with_irqs_disabled;
use spinning_top::Spinlock;

use crate::process::Process;

static TABLE: Spinlock<Table> = Spinlock::new(Table::new());

/// `TABLE`'s length, mirrored under the hold so [`active`] is a load, not a
/// lock: it is asked once per `write(2)` on the whole system.
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
fn fill_begin(inode: u32) -> u64 {
    FILLS.fetch_add(1, Ordering::SeqCst);
    GENS.read(inode)
}

fn fill_end() {
    FILLS.fetch_sub(1, Ordering::SeqCst);
}

/// What became of a fill.
enum Fill {
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

/// Publish a filled frame — unless a write raced the fill (see
/// `akuma_fpcache_rw::Generations`), checked under the same hold the writer's
/// copy takes. `None` publishes without the check, after the retry bound.
fn publish(key: Key, pa: usize, generation: Option<u64>) -> Fill {
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

/// [`reap`] the entries in a file range whose frames a bulk unmap released.
///
/// Every entry of `(mount_id, inode)` in file bytes `[from, to)` whose
/// frame is in `released` (sorted): the frames a bulk unmap just gave up, which
/// knows frames but not their keys. Only those, never a blanket sweep — an entry
/// nobody maps may hold a killed mapper's unflushed stores, and the table is
/// the only place they live. The caller wrote its own pages back first.
pub fn reap_released(mount_id: u32, inode: u32, from: usize, to: usize, released: &[usize]) {
    if released.is_empty() || LEN.load(Ordering::SeqCst) == 0 {
        return;
    }
    let mut keys: alloc::vec::Vec<(Key, usize)> = alloc::vec::Vec::new();
    with_irqs_disabled(|| {
        TABLE.lock().for_each_in(mount_id, inode, from, to, |page_off, pa| {
            if released.binary_search(&pa).is_ok() {
                keys.push((Key::new(mount_id, inode, page_off), pa));
            }
        });
    });
    for (key, pa) in keys {
        reap(key, pa);
    }
}

/// `akuma_vfs_glue::MappedFileHooks::active`.
pub fn active() -> bool {
    FILLS.load(Ordering::SeqCst) > 0 || LEN.load(Ordering::SeqCst) > 0
}

/// `akuma_vfs_glue::MappedFileHooks::wrote`: copy a file write into every
/// mapped frame it overlaps, so a mapper sees it as Linux's page cache would.
///
/// **A flush is a write too**, and its source can be the frame: copying that
/// onto itself is not a no-op under concurrency — a peer's store landing
/// between the copy's load and its store would be undone — so a span whose
/// source is the destination is skipped.
pub fn wrote(mount_id: u32, inode: u32, offset: usize, data: &[u8]) {
    GENS.bump(inode);
    let mut copied = false;
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
            copied |= akuma_mmu::copy_to_phys(pa + s.frame_off, src);
        });
    });
    if copied {
        WRITE_THROUGHS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `akuma_vfs_glue::MappedFileHooks::zeroed`: bytes `[from, to)` of the file
/// read zero now (truncate, hole punch) — and so must the mapped frames.
pub fn zeroed(mount_id: u32, inode: u32, from: usize, to: usize) {
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

/// Drop any entry of `inode` a missed reap left behind. Called from ext2's
/// inode-freed hook: a mapping pins its inode, so by the time the number is
/// freed nothing can map these frames.
pub fn inode_freed(inode: u32) {
    if LEN.load(Ordering::SeqCst) == 0 {
        return;
    }
    with_table(|t| {
        t.detach_inode(inode, |pa| akuma_pmm::free_page(pa, 0));
    });
}

/// One line for diagnostics, written into the caller's buffer.
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

/// Map the table's frame `pa` at `va`, as the region says, never CoW-marked.
///
/// A write that broke "sharing" here would be the
/// incoherence the table exists to remove.
///
/// Arrives holding one reference for this mapping, and either installs it (the
/// reference becomes the address space's) or gives it back. **A present leaf is
/// never overwritten**: a peer thread won the fault, and overwriting would
/// strand the reference of whatever it mapped.
fn map_shared(proc: &Process, va: usize, flags: u64, pa: usize) -> bool {
    let frame = crate::PhysFrame::new(pa);
    let (mapped, release) = proc.with_address_space(|a| {
        if a.is_mapped(va) {
            return (true, true);
        }
        // `true`: the caller's reference. Reported back as surplus when this
        // address space already holds the frame at another VA, because teardown
        // frees each distinct frame once.
        let surplus = a.adopt_user_frame(frame, true);
        if a.map_page(va, pa, flags).is_ok() {
            (true, surplus)
        } else {
            let _ = a.remove_user_frame(frame);
            (false, true)
        }
    });
    if release {
        akuma_pmm::free_page(pa, 0);
    }
    mapped
}

/// The table's key for the page at `va` of `proc`, from its region list.
///
/// `(key, true)` when the region is a writable `MAP_SHARED` file mapping (its
/// pages *are* the table's), `(key, false)` for a page of any other file
/// mapping, the one straddling EOF included (it must see a writer's frame
/// while one exists, as amd64's fault path always has). `None` for
/// anything else, including an ELF segment (`lead`), which no one maps
/// shared-writable.
pub fn region_key(proc: &Process, va: usize) -> Option<(Key, bool)> {
    proc.vm_with_regions(|r| {
        let reg = &r[crate::process::region_index_containing(r, va)?];
        let idx = (va - reg.start_va) / 4096;
        if let Some(sw) = &reg.shared_write {
            return Some((Key::new(sw.mount_id, sw.inode, sw.offset + idx * 4096), true));
        }
        let f = reg.file.filter(|f| f.lead == 0)?;
        Some((Key::new(f.mount_id, f.inode, f.page_source(idx).0), false))
    })
}

/// If some process maps page `key` shared-writable, map **its** frame at `va`.
///
/// Mapped with `flags`; says whether that worked, `None` when there is no entry.
///
/// The fault path of a *read-only* mapping of a file asks this first: until the
/// writer flushes, the table's frame is the page and the file is stale, and a
/// reader must see what Linux's one page cache would give it. Chromium's font
/// service fills a shared-memory file through a writable mapping and hands the
/// renderer a read-only one (`shmregionprobe.c`). One atomic load when no
/// process maps a file shared-writable.
pub fn map_existing(proc: &Process, va: usize, flags: u64, key: Key) -> Option<bool> {
    if !active() {
        return None;
    }
    let pa = lookup_and_ref(key)?;
    let ok = map_shared(proc, va, flags, pa);
    // Speculative: cleans up the one case that leaves an entry with no mapper
    // (this mapping failed while every other mapper left).
    reap(key, pa);
    Some(ok)
}

/// Make page `key` present at `va` in `proc`'s address space from the table.
///
/// A hit if a peer maps it, otherwise a fill from the file that this call
/// publishes. `flags` is the region's protection as user PTE flags.
///
/// The page is the file's **current** bytes, whole: not clamped by a size
/// captured at `mmap`, which is wrong for a file that grew since (SQLite grows
/// its `-shm` and maps the new part later). Published before it is mapped, so a
/// peer faulting the same page lands on this frame rather than filling its own.
///
/// # The fill race
///
/// A `write(2)` between the file read and the publish would find no frame to
/// copy into, and the page published afterwards would be older than the file.
/// `fill_begin`/`publish` detect it and the page is read again — bounded, since
/// a generation bucket shared with a busy unrelated file can keep moving. After
/// the bound the frame is published anyway and counted in `FILL_RETRIES`.
pub fn fault_page(proc: &Process, va: usize, flags: u64, key: Key) -> bool {
    const FILL_ATTEMPTS: usize = 8;
    if let Some(ok) = map_existing(proc, va, flags, key) {
        return ok;
    }
    let Some(frame) = akuma_pmm::alloc_page_zeroed_user() else {
        return false;
    };
    let read = crate::runtime::runtime().read_at_by_inode;
    let mut attempt = 0;
    let outcome = loop {
        attempt += 1;
        let generation = fill_begin(key.inode);
        // Nothing can reach the frame until `publish`. Zeroed before every
        // read: zero is the value of a byte past EOF, and a retry must not keep
        // a previous attempt's tail.
        let filled = akuma_mmu::with_phys_bytes_mut(frame, 0, 4096, |bytes| {
            bytes.fill(0);
            read(key.mount_id, key.inode, key.offset, bytes).is_ok()
        })
        .unwrap_or(false);
        let outcome = filled.then(|| publish(key, frame, (attempt < FILL_ATTEMPTS).then_some(generation)));
        fill_end();
        if !matches!(outcome, Some(Fill::Stale)) {
            break outcome;
        }
    };
    match outcome {
        Some(Fill::Inserted) => {
            let ok = map_shared(proc, va, flags, frame);
            reap(key, frame);
            ok
        }
        Some(Fill::Existing(pa)) => {
            // A peer published first: this frame was never referenced, so it
            // frees outright, and the peer's is the page.
            akuma_pmm::free_page(frame, 0);
            let ok = map_shared(proc, va, flags, pa);
            reap(key, pa);
            ok
        }
        // Not shareable: a private page, holding the allocation's reference.
        Some(Fill::Refused) => map_shared(proc, va, flags, frame),
        // The file stopped answering (nothing was published), or a `Stale` the
        // loop cannot leave on — an arm rather than a panic in a fault handler.
        None | Some(Fill::Stale) => {
            akuma_pmm::free_page(frame, 0);
            false
        }
    }
}
