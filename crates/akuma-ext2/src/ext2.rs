//! Ext2 Filesystem Implementation (crate-internal module).

// Ext2 on-disk structures require raw pointer casts and low-level arithmetic
// that trigger many pedantic/nursery lints. Suppress the noisy ones here.
#![allow(
    clippy::cast_lossless,
    clippy::ptr_as_ptr,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::doc_markdown,
    clippy::significant_drop_tightening,
    clippy::question_mark,
    clippy::collapsible_if,
    clippy::result_unit_err,
    clippy::unused_self,
    clippy::if_then_some_else_none,
    clippy::manual_div_ceil,
    clippy::str_to_string,
    clippy::needless_borrows_for_generic_args,
    clippy::return_self_not_must_use,
    clippy::unnecessary_struct_initialization,
    clippy::derivable_impls,
    clippy::needless_pass_by_value,
    clippy::ignored_unit_patterns,
    clippy::needless_collect,
    clippy::unnecessary_literal_bound,
    clippy::use_self,
    clippy::manual_repeat_n,
    clippy::missing_const_for_fn,
    clippy::no_effect_underscore_binding,
    clippy::manual_is_multiple_of,
    clippy::redundant_pub_crate,
)]

#[cfg(any(ext2_fs_cache, test))]
use alloc::collections::BTreeMap;
#[cfg(any(ext2_fs_cache, test))]
use core::cell::Cell;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
#[cfg(not(kernel_profile_extreme))]
use spinning_top::Spinlock;

use akuma_locks_rw_cell::RecoverableCell;
use akuma_primitives::Registered;
use akuma_vfs::{DirEntry, Filesystem, FsError, FsStats, Metadata, path_components, split_path};
use crate::BlockDevice;

/// Number of block slots in the ring cache. One contiguous backing allocation;
/// linear-scan lookup is fine at this size (fits in L1).
#[cfg(all(not(kernel_profile_extreme), not(ext2_fs_cache)))]
const BLOCK_CACHE_ENTRIES: usize = 64;

/// Flat ring-buffer block cache. A single contiguous Vec<u8> holds all cached
/// block data; a parallel tag array records which block number occupies each
/// slot (u32::MAX = empty). Eviction is pure ring — no LRU bookkeeping.
///
/// Compared to the old BTreeMap<u32, Vec<u8>>: one heap allocation instead of
/// N, so when the cache is dropped or pressure forces reclaim, the whole backing
/// buffer frees in one shot rather than leaving N scattered Vec headers across
/// the PMM-claimed heap span.
///
/// **Write-back**: slots carry a dirty bit; `write`/`patch` update the slot and
/// defer the device write to `flush_dirty` (called from `flush_meta`/`sync`,
/// see `docs/archive/EXT2_WRITEBACK_DESIGN.md`). Evicting a dirty victim
/// flushes it first via the caller-supplied device callback. `remove` drops a
/// block *without* flushing — the invalidate-on-free rule (D-3): a freed
/// block's stale bytes must never reach a disk block that may have been
/// reallocated.
#[cfg(all(not(kernel_profile_extreme), not(ext2_fs_cache)))]
struct BlockRingCache {
    backing: Vec<u8>,
    tags: [u32; BLOCK_CACHE_ENTRIES],
    dirty: [bool; BLOCK_CACHE_ENTRIES],
    head: usize,
    block_size: usize,
}

/// Device-write callback for cache flushes (eviction and `flush_dirty`).
/// `FnMut(block_num, bytes) -> device write result` — callers pass a closure
/// over `&self.dev`, which is field-disjoint from the `block_cache` lock.
#[cfg(not(kernel_profile_extreme))]
pub(crate) type DevFlush<'a> = &'a mut dyn FnMut(u32, &[u8]) -> Result<(), FsError>;

#[cfg(all(not(kernel_profile_extreme), not(ext2_fs_cache)))]
impl BlockRingCache {
    fn new(block_size: usize) -> Self {
        Self {
            backing: vec![0u8; BLOCK_CACHE_ENTRIES * block_size],
            tags: [u32::MAX; BLOCK_CACHE_ENTRIES],
            dirty: [false; BLOCK_CACHE_ENTRIES],
            head: 0,
            block_size,
        }
    }

    fn get(&self, block_num: u32) -> Option<&[u8]> {
        for (i, &tag) in self.tags.iter().enumerate() {
            if tag == block_num {
                let s = i * self.block_size;
                return Some(&self.backing[s..s + self.block_size]);
            }
        }
        None
    }

    fn slot_of(&self, block_num: u32) -> Option<usize> {
        self.tags.iter().position(|&tag| tag == block_num)
    }

    /// Write a block into the cache, marking it dirty (device write deferred).
    /// If the ring slot it lands in holds a dirty victim, that victim is
    /// flushed first — a dirty block must never be silently overwritten.
    fn write(&mut self, block_num: u32, data: &[u8], flush: DevFlush<'_>) -> Result<(), FsError> {
        if let Some(i) = self.slot_of(block_num) {
            let s = i * self.block_size;
            self.backing[s..s + self.block_size].copy_from_slice(data);
            self.dirty[i] = true;
            return Ok(());
        }
        let slot = self.head;
        self.evict_into(slot, flush)?;
        let s = slot * self.block_size;
        self.backing[s..s + self.block_size].copy_from_slice(data);
        self.tags[slot] = block_num;
        self.dirty[slot] = true;
        self.head = (self.head + 1) % BLOCK_CACHE_ENTRIES;
        Ok(())
    }

    /// Overwrite a sub-block range of a resident block, marking it dirty.
    /// Returns `false` (no-op) when the block is not cached — the caller
    /// falls back to fill-then-patch.
    fn patch(&mut self, block_num: u32, off: usize, data: &[u8]) -> bool {
        match self.slot_of(block_num) {
            Some(i) => {
                let s = i * self.block_size + off;
                self.backing[s..s + data.len()].copy_from_slice(data);
                self.dirty[i] = true;
                true
            }
            None => false,
        }
    }

    /// Prepare `slot` for a new occupant, flushing its dirty predecessor.
    fn evict_into(&mut self, slot: usize, flush: DevFlush<'_>) -> Result<(), FsError> {
        let victim = self.tags[slot];
        if victim != u32::MAX && self.dirty[slot] {
            let s = slot * self.block_size;
            flush(victim, &self.backing[s..s + self.block_size])?;
        }
        self.dirty[slot] = false;
        Ok(())
    }

    /// Insert a clean block (device read fill). Same duplicate and dirty-victim
    /// rules as [`Self::write`].
    fn insert(&mut self, block_num: u32, data: &[u8], flush: DevFlush<'_>) -> Result<(), FsError> {
        // If already cached (another thread beat us to it), don't add a duplicate.
        // Duplicates would let a stale copy survive a remove() that only clears
        // the first match, which would poison later reads after a write.
        if self.tags.contains(&block_num) {
            return Ok(());
        }
        let slot = self.head;
        self.evict_into(slot, flush)?;
        let s = slot * self.block_size;
        self.backing[s..s + self.block_size].copy_from_slice(data);
        self.tags[slot] = block_num;
        self.dirty[slot] = false;
        self.head = (self.head + 1) % BLOCK_CACHE_ENTRIES;
        Ok(())
    }

    /// Drop a block without flushing, dirty or not (invalidate-on-free, D-3).
    fn remove(&mut self, block_num: u32) {
        for (i, tag) in self.tags.iter_mut().enumerate() {
            if *tag == block_num {
                *tag = u32::MAX;
                self.dirty[i] = false;
                return;
            }
        }
    }

    /// Is `block_num` resident with unflushed bytes? The `[E2C-BAD]` oracle
    /// skips dirty blocks: cache-ahead-of-disk is the write-back invariant,
    /// not a coherence failure.
    fn is_dirty(&self, block_num: u32) -> bool {
        match self.slot_of(block_num) {
            Some(i) => self.dirty[i],
            None => false,
        }
    }

    /// Write every dirty block whose number passes `keep` out to the device
    /// (via `flush`), clearing the bits as they land. `keep` implements the
    /// flush ordering (D-2): data/inode blocks first, allocation metadata
    /// (bitmaps, BGD table) only after.
    fn flush_dirty(&mut self, keep: &dyn Fn(u32) -> bool, flush: DevFlush<'_>) -> Result<(), FsError> {
        for i in 0..BLOCK_CACHE_ENTRIES {
            let tag = self.tags[i];
            if tag != u32::MAX && self.dirty[i] && keep(tag) {
                let s = i * self.block_size;
                flush(tag, &self.backing[s..s + self.block_size])?;
                self.dirty[i] = false;
            }
        }
        Ok(())
    }
}

// ============================================================================
// Large clock-eviction block cache (feature `fs-cache` / cfg `ext2_fs_cache`)
// ============================================================================
//
// The 64-slot ring above is ~256 KB — far smaller than a self-host build's
// working set (the toolchain `.so`s + rlibs are a few hundred MB), so it gives
// no reuse: every rustc/cc/ld spawn re-streams the toolchain off virtio-blk
// (docs/AKUMA_SELF_HOSTING.md §7a — measured warm/cold ratio = 1.00x).
//
// This cache is sized from detected RAM (capped, set by the kernel before mount)
// and evicts with a CLOCK (second-chance) policy so frequently-touched toolchain
// blocks stay resident even while cold blocks stream past — a pure ring would
// evict the hot set as soon as the working set exceeds the cache. Lookup is
// O(log n) via a `block_num -> slot` BTreeMap (a linear scan over ~131 072 slots
// per block read would dwarf the disk read it is trying to avoid).

/// Default cap if the kernel never calls [`set_cache_cap_bytes`] (host tests):
/// 16 MB — large enough to exercise eviction, small enough for `cargo test`.
#[cfg(any(ext2_fs_cache, test))]
const DEFAULT_CACHE_CAP_BYTES: usize = 16 * 1024 * 1024;

#[cfg(any(ext2_fs_cache, test))]
static CACHE_CAP_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_CACHE_CAP_BYTES);

/// Cache instrumentation for the boot self-test (cache_hit_test) and PSTATS.
#[cfg(any(ext2_fs_cache, test))]
static CACHE_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(any(ext2_fs_cache, test))]
static CACHE_MISSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Slots currently allocated / total slots the cap allows. Occupancy is what
/// distinguishes "the cache is too small and thrashing" from "the working set
/// fits and misses are just cold-start": a cache that never fills cannot be
/// improved by raising the cap, whatever the miss count says.
#[cfg(any(ext2_fs_cache, test))]
static CACHE_SLOTS_USED: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(ext2_fs_cache, test))]
static CACHE_SLOTS_CAP: AtomicUsize = AtomicUsize::new(0);

/// Set the upper bound (bytes) on the ext2 block cache backing store.
///
/// The kernel derives this from detected RAM — `min(RAM/8, 128 MB)` as of
/// `src/fs.rs` — and calls it once before mounting the filesystem. No-op unless
/// built with the `fs-cache` feature (which is in the kernel's `default` set).
#[allow(unused_variables)]
pub fn set_cache_cap_bytes(bytes: usize) {
    #[cfg(any(ext2_fs_cache, test))]
    CACHE_CAP_BYTES.store(bytes, Ordering::Relaxed);
}

/// `(hits, misses)` since boot for the large block cache. `(0, 0)` unless the
/// `fs-cache` feature is built in.
#[must_use]
pub fn cache_stats() -> (u64, u64) {
    #[cfg(any(ext2_fs_cache, test))]
    {
        (CACHE_HITS.load(Ordering::Relaxed), CACHE_MISSES.load(Ordering::Relaxed))
    }
    #[cfg(not(any(ext2_fs_cache, test)))]
    {
        (0, 0)
    }
}

/// `(slots_in_use, slot_capacity)` for the large block cache.
///
/// Equal values mean the cache is full and the clock is evicting;
/// `slots_in_use` well under capacity means the whole touched working set is
/// resident and a larger cap would buy nothing. `(0, 0)` unless built with
/// `fs-cache`.
#[must_use]
pub fn cache_occupancy() -> (usize, usize) {
    #[cfg(any(ext2_fs_cache, test))]
    {
        (CACHE_SLOTS_USED.load(Ordering::Relaxed), CACHE_SLOTS_CAP.load(Ordering::Relaxed))
    }
    #[cfg(not(any(ext2_fs_cache, test)))]
    {
        (0, 0)
    }
}

/// Target size of one backing chunk. The backing grows a whole chunk at a time,
/// so this is the largest single allocation the cache ever asks for, regardless
/// of `capacity_blocks`.
///
/// **Sized 64 KB, not 1 MB (2026-09-03).** A kernel-heap span must be
/// *physically* contiguous (the heap lives in the `phys_to_virt` linear map), so
/// a 1 MB chunk demanded a 256-page run from a PMM that a running build has
/// checkerboarded. Measured at `MEMORY=2048`: `[OOM] allocation of 1048576 bytes
/// failed` with **893 MB still free** — `handle_oom`'s "true fragmentation OOM"
/// path, which kills a process. 64 KB needs a 16-page run instead, and costs
/// only a longer `chunks` vector (16x more entries, 8 bytes each). The cap and
/// the total footprint are unchanged.
///
/// A single contiguous `Vec<u8>` was the original design and it does not scale:
/// `Vec` growth doubles, so a 512 MB cap meant a `realloc(256 MB -> 512 MB)` with
/// both buffers live (~768 MB transient) and a 512 MB contiguous demand on the
/// kernel heap. Observed 2026-08-02 on the rustc benchmark: `[HEAP-GROW]
/// total=1152MB ... claimed=131074 pages`, PMM 908 518 -> 678 073 free pages
/// (~900 MB, never returned), after which sshd accepted connections but reset at
/// key exchange. Chunking bounds the allocation and never copies existing slots.
#[cfg(any(ext2_fs_cache, test))]
const CACHE_CHUNK_BYTES: usize = 1 << 16; // 64 KB

/// Clock (second-chance) block cache. Slots are allocated lazily (one backing
/// chunk at a time) up to `capacity_blocks`; thereafter the clock hand sweeps,
/// clearing reference bits, and evicts the first unreferenced slot.
///
/// **Write-back**: same dirty-bit contract as [`BlockRingCache`] — see the
/// struct docs there and `docs/archive/EXT2_WRITEBACK_DESIGN.md`.
#[cfg(any(ext2_fs_cache, test))]
pub(crate) struct ClockBlockCache {
    /// Slot data, split into fixed-size chunks of `chunk_blocks` slots each.
    /// Slot `i` lives in `chunks[i / chunk_blocks]` at byte offset
    /// `(i % chunk_blocks) * block_size`. Never reallocated once pushed, so a
    /// grow costs one `CACHE_CHUNK_BYTES` allocation and zero copying.
    chunks: Vec<Vec<u8>>,
    /// Slots per chunk; `>= 1` even when `block_size > CACHE_CHUNK_BYTES`.
    chunk_blocks: usize,
    /// Block number occupying each slot; `u32::MAX` = empty hole.
    tags: Vec<u32>,
    /// Clock reference bit per slot. `Cell` so a read (`get`) can set the bit
    /// through `&self` (it runs under the cache's spinlock, so no real sharing).
    ref_bits: Vec<Cell<bool>>,
    /// Write-back dirty bit per slot. Plain `bool`: only `&mut self` methods
    /// touch it, unlike `ref_bits`.
    dirty_bits: Vec<bool>,
    /// `block_num -> slot` for O(log n) lookup.
    index: BTreeMap<u32, usize>,
    hand: usize,
    block_size: usize,
    capacity_blocks: usize,
}

#[cfg(any(ext2_fs_cache, test))]
impl ClockBlockCache {
    pub(crate) fn new(block_size: usize) -> Self {
        let cap_bytes = CACHE_CAP_BYTES.load(Ordering::Relaxed);
        // At least the old ring's worth of slots; never zero.
        let capacity_blocks = core::cmp::max(64, cap_bytes / block_size.max(1));
        Self::with_capacity_blocks(block_size, capacity_blocks)
    }

    /// Construct with an explicit slot capacity (tests use this to avoid racing
    /// on the global `CACHE_CAP_BYTES` when run in parallel).
    pub(crate) fn with_capacity_blocks(block_size: usize, capacity_blocks: usize) -> Self {
        CACHE_SLOTS_CAP.store(capacity_blocks, Ordering::Relaxed);
        Self {
            chunks: Vec::new(),
            chunk_blocks: core::cmp::max(1, CACHE_CHUNK_BYTES / block_size.max(1)),
            tags: Vec::new(),
            ref_bits: Vec::new(),
            dirty_bits: Vec::new(),
            index: BTreeMap::new(),
            hand: 0,
            block_size,
            capacity_blocks,
        }
    }

    /// Byte range of `slot` within its chunk.
    fn slot_pos(&self, slot: usize) -> (usize, usize) {
        (slot / self.chunk_blocks, (slot % self.chunk_blocks) * self.block_size)
    }

    pub(crate) fn get(&self, block_num: u32) -> Option<&[u8]> {
        if let Some(&slot) = self.index.get(&block_num) {
            self.ref_bits[slot].set(true);
            let (chunk, off) = self.slot_pos(slot);
            Some(&self.chunks[chunk][off..off + self.block_size])
        } else {
            None
        }
    }

    /// Pick a slot for a new block: grow the backing while under capacity —
    /// pushing at most one `CACHE_CHUNK_BYTES` chunk, and only when the current
    /// chunk is full — otherwise run the clock. A dirty victim is flushed
    /// through `flush` before its slot is reused.
    fn alloc_slot(&mut self, flush: DevFlush<'_>) -> Result<usize, FsError> {
        let slots = self.tags.len();
        if slots < self.capacity_blocks {
            if slots % self.chunk_blocks == 0 {
                // New chunk needed. Its size is fixed, so heap demand never
                // scales with capacity_blocks and existing slots never move.
                self.chunks.push(vec![0u8; self.chunk_blocks * self.block_size]);
            }
            self.tags.push(u32::MAX);
            self.ref_bits.push(Cell::new(false));
            self.dirty_bits.push(false);
            CACHE_SLOTS_USED.store(self.tags.len(), Ordering::Relaxed);
            return Ok(slots);
        }
        loop {
            if self.hand >= slots {
                self.hand = 0;
            }
            if self.ref_bits[self.hand].get() {
                self.ref_bits[self.hand].set(false);
                self.hand = (self.hand + 1) % slots;
            } else {
                let victim_slot = self.hand;
                let victim = self.tags[victim_slot];
                if victim != u32::MAX && self.dirty_bits[victim_slot] {
                    let (chunk, off) = self.slot_pos(victim_slot);
                    let bs = self.block_size;
                    flush(victim, &self.chunks[chunk][off..off + bs])?;
                }
                self.hand = (victim_slot + 1) % slots;
                return Ok(victim_slot);
            }
        }
    }

    /// Occupy `slot` with `block_num`/`data` at dirtiness `dirty`, evicting the
    /// slot's prior occupant from the index.
    fn occupy(&mut self, slot: usize, block_num: u32, data: &[u8], dirty: bool) {
        let prev = self.tags[slot];
        if prev != u32::MAX {
            self.index.remove(&prev);
        }
        let (chunk, off) = self.slot_pos(slot);
        let bs = self.block_size;
        self.chunks[chunk][off..off + bs].copy_from_slice(data);
        self.tags[slot] = block_num;
        self.ref_bits[slot].set(true);
        self.dirty_bits[slot] = dirty;
        self.index.insert(block_num, slot);
    }

    /// Insert a clean block (device read fill). No duplicates; a dirty eviction
    /// victim is flushed first.
    pub(crate) fn insert(&mut self, block_num: u32, data: &[u8], flush: DevFlush<'_>) -> Result<(), FsError> {
        // Already present (another thread beat us, or a re-read): don't duplicate.
        if self.index.contains_key(&block_num) {
            return Ok(());
        }
        let slot = self.alloc_slot(flush)?;
        self.occupy(slot, block_num, data, false);
        Ok(())
    }

    /// Write a block's bytes into the cache, marking it dirty (write-back).
    pub(crate) fn write(&mut self, block_num: u32, data: &[u8], flush: DevFlush<'_>) -> Result<(), FsError> {
        if let Some(&slot) = self.index.get(&block_num) {
            let (chunk, off) = self.slot_pos(slot);
            let bs = self.block_size;
            self.chunks[chunk][off..off + bs].copy_from_slice(data);
            self.dirty_bits[slot] = true;
            self.ref_bits[slot].set(true);
            return Ok(());
        }
        let slot = self.alloc_slot(flush)?;
        self.occupy(slot, block_num, data, true);
        Ok(())
    }

    /// Overwrite a sub-block range of a resident block, marking it dirty.
    /// `false` (no-op) when not cached — caller falls back to fill-then-patch.
    pub(crate) fn patch(&mut self, block_num: u32, off: usize, data: &[u8]) -> bool {
        match self.index.get(&block_num) {
            Some(&slot) => {
                let (chunk, base) = self.slot_pos(slot);
                self.chunks[chunk][base + off..base + off + data.len()].copy_from_slice(data);
                self.dirty_bits[slot] = true;
                self.ref_bits[slot].set(true);
                true
            }
            None => false,
        }
    }

    /// Drop a block without flushing, dirty or not (invalidate-on-free, D-3).
    pub(crate) fn remove(&mut self, block_num: u32) {
        if let Some(slot) = self.index.remove(&block_num) {
            self.tags[slot] = u32::MAX;
            self.ref_bits[slot].set(false);
            self.dirty_bits[slot] = false;
        }
    }

    /// Is `block_num` resident with unflushed bytes? (See [`BlockRingCache::is_dirty`].)
    pub(crate) fn is_dirty(&self, block_num: u32) -> bool {
        match self.index.get(&block_num) {
            Some(&slot) => self.dirty_bits[slot],
            None => false,
        }
    }

    /// Flush dirty blocks passing `keep` to the device, in slot order (D-2).
    pub(crate) fn flush_dirty(
        &mut self,
        keep: &dyn Fn(u32) -> bool,
        flush: DevFlush<'_>,
    ) -> Result<(), FsError> {
        for slot in 0..self.tags.len() {
            let tag = self.tags[slot];
            if tag != u32::MAX && self.dirty_bits[slot] && keep(tag) {
                let (chunk, off) = self.slot_pos(slot);
                let bs = self.block_size;
                flush(tag, &self.chunks[chunk][off..off + bs])?;
                self.dirty_bits[slot] = false;
            }
        }
        Ok(())
    }
}

/// Diagnostic (2026-08-15 zero-page hunt): cache hits whose bytes did not match
/// a direct disk re-read (`[E2C-BAD]`). Non-zero means this layer is serving
/// stale data — the "wrong bytes" half of the self-host ICE residue.
/// Gated with its only consumer (`verify_cached_block`): `extreme` has no block
/// cache at all, so there is nothing to verify and the counter is dead there.
#[cfg(not(kernel_profile_extreme))]
pub static E2_CACHE_VERIFY_MISMATCH: AtomicUsize = AtomicUsize::new(0);

/// Runtime gate for the `[E2C-BAD]` verification — **default off**, and that is
/// load-bearing, not a nicety: re-reading every cache hit from disk doubles I/O
/// on the hottest path in the self-host build and serialises exactly the
/// interleavings a coherence race needs. The first instrumented arm went 4/4
/// green with it on and proved nothing (hunt §13, handoff rule 10). Turn it on
/// only when actively chasing T4, and never trust a rate scored with it on.
#[cfg(not(kernel_profile_extreme))]
pub static E2_VERIFY_HITS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Diagnostic (2026-08-15 zero-page hunt): `read_at_by_inode` calls that hit the
/// EOF arm with a non-zero offset (`[E2-EOF]`) — the caller's mmap-time `filesz`
/// said the file extended further than the inode's size says now. Normal EOFs
/// (pread at end of file) land here too, so this is a *rate* signal: a burst
/// correlated with `[FILL-SHORT]` is the defect.
pub static E2_READ_AT_EOF: AtomicUsize = AtomicUsize::new(0);

// ==========================================================================
// Deferred inode frees — "unlinked but still mapped"
// ==========================================================================

/// Slots for inodes unlinked while still mapped.
///
/// Sized for the worst case observed in a `-j4` self-host build, where the whole
/// deferred set drains within a handful of unlinks (`cargo` unlinks ~1000 files
/// per build, and every one of them drains this list first).
///
/// A fixed array of atomics rather than a `Vec` because it is manipulated under
/// the state write lock on paths that must not allocate, and because a bounded
/// structure cannot itself become the reason a free is lost.
const DEFERRED_FREE_SLOTS: usize = 256;

/// The per-filesystem list of inodes whose last name was removed while a mapping
/// still held them pinned. The dirent is gone, but the inode keeps its size and
/// block pointers so the mapping goes on reading real data; the truncate and the
/// bitmap free happen in [`Ext2Filesystem::drain_deferred_frees`] once the last
/// pin drops. `0` means an empty slot.
///
/// **Per-filesystem, not global**, and that is a correctness requirement rather
/// than tidiness: an inode number only means something relative to the
/// filesystem that issued it, so a shared list would let one mount's drain free
/// an unrelated inode on another. (The global version of this list failed
/// exactly that way against the test suite's parallel mounts.)
struct DeferredFrees {
    slots: [AtomicU32; DEFERRED_FREE_SLOTS],
}

impl DeferredFrees {
    const fn new() -> Self {
        Self { slots: [const { AtomicU32::new(0) }; DEFERRED_FREE_SLOTS] }
    }

    /// Record `inode` for a later free. Returns `false` if there was no room, in
    /// which case the caller must leak it rather than free it.
    fn push(&self, inode: u32) -> bool {
        for slot in &self.slots {
            if slot.load(Ordering::Relaxed) != 0 {
                continue;
            }
            if slot.compare_exchange(0, inode, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                DEFERRED_FREE_PENDING.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        DEFERRED_FREE_LEAKED.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// Inodes currently awaiting a deferred free, across every mount, for the
/// `[Mem]` dump. Tracked as a counter because the lists themselves are
/// per-filesystem and the dump has no filesystem in hand.
pub static DEFERRED_FREE_PENDING: AtomicUsize = AtomicUsize::new(0);

/// Inodes leaked rather than freed, because they were unlinked while pinned and
/// the deferral list was full.
///
/// Leaked blocks are recoverable (`e2fsck` reconnects them to `lost+found`);
/// bytes handed to the wrong reader are not, which is why this is the direction
/// the overflow falls. Non-zero here means the bound needs raising.
pub static DEFERRED_FREE_LEAKED: AtomicUsize = AtomicUsize::new(0);

/// Inodes currently awaiting their deferred free, for the `[Mem]`/PSTATS dump.
#[must_use]
pub fn deferred_free_pending() -> usize {
    DEFERRED_FREE_PENDING.load(Ordering::Relaxed)
}

/// The thread hooks are **gone** (2026-08-31, `AKUMA_EXT2_CLEANUP.md` §4.4).
///
/// `init_thread_hooks(current_thread_id, is_thread_dead)` existed only to serve
/// the orphaned-write-lock recovery: the acquisition loops recorded a tid, and
/// every 10 000 spins asked the scheduler whether that tid was still alive so
/// they could `force_unlock_write()` a third-party lock. Both questions are now
/// somebody else's — the acquire-side identity is read natively by
/// `akuma_primitives::preempt::current_tid()` inside the lock, and liveness is
/// never *asked*: the runtime *reports* a death by calling
/// [`Ext2Filesystem::abandon_tid`]. This crate no longer names a tid at all.
/// Kernel callback invoked when an inode number is returned to the allocator.
///
/// The kernel's `file_page_cache` is keyed on **`(inode, file_offset)`**, so a
/// recycled inode number silently inherits the previous file's cached pages
/// unless they are dropped at the moment the number is released. Path-keyed
/// invalidation at `unlink` is not enough: a mapping of an unlinked file goes on
/// faulting and publishing pages under that number right up until its last
/// reference drops.
///
/// This was not hypothetical. Deferring the free (so unlinked-but-mapped files
/// keep their data) removed an *accidental* protection: before it, those fills
/// hit the truncated inode, returned `Ok(0)` and were withheld from the cache by
/// `fill_complete`. Once they started succeeding, the stale entries became
/// reachable and the next file to take the number read the dead file's bytes —
/// which showed up as `rust-lld: ELF section name out of range` on a freshly
/// built `libsyn.rlib`. See `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §15.
#[derive(Clone, Copy)]
struct InodeFreedHook {
    invalidate_pages: fn(u32),
}

static INODE_FREED_HOOK: Registered<InodeFreedHook> =
    Registered::new("akuma-ext2: InodeFreedHook not registered");

/// Register the "this inode number is being reissued" callback.
///
/// Called once by the kernel at boot; unregistered (host tests, early boot)
/// degrades to a no-op, which is correct because neither has a page cache.
pub fn init_inode_freed_hook(invalidate_pages: fn(u32)) {
    INODE_FREED_HOOK.register(InodeFreedHook { invalidate_pages });
}

fn on_inode_freed(inode: u32) {
    if let Some(h) = INODE_FREED_HOOK.get() {
        (h.invalidate_pages)(inode);
    }
}

// ============================================================================
// Constants
// ============================================================================

/// Ext2 superblock magic number
const EXT2_MAGIC: u16 = 0xEF53;

/// Superblock offset from start of disk (always 1024 bytes)
const SUPERBLOCK_OFFSET: u64 = 1024;

/// Root directory inode number
const ROOT_INODE: u32 = 2;

/// File type constants (from inode type_perms field)
const S_IFREG: u16 = 0x8000; // Regular file
const S_IFDIR: u16 = 0x4000; // Directory
const S_IFLNK: u16 = 0xA000; // Symbolic link
/// Unix-domain socket node. Created by `bind(2)` on an AF_UNIX pathname; it
/// carries no data, only the type bits.
///
/// Those bits are the whole point. A client connecting to a unix socket
/// `stat`s the path and checks `S_ISSOCK` first — before this existed, `bind`
/// created an ordinary file and `stat` reported `S_IFREG`, so a conformant
/// client refused to connect to a socket that was working perfectly. Caught by
/// `nettest-unix path` diffing against its Linux control arm
/// (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md` G7).
const S_IFSOCK: u16 = 0xC000; // Unix-domain socket

/// Default permissions for new files/directories
const DEFAULT_FILE_PERMS: u16 = S_IFREG | 0o644;
const DEFAULT_DIR_PERMS: u16 = S_IFDIR | 0o755;
const DEFAULT_SYMLINK_PERMS: u16 = S_IFLNK | 0o777;
/// A socket node is `srwxr-xr-x`: the permission bits are what govern who may
/// `connect`, and Linux applies the process umask to `0o777` here. There is no
/// per-process umask in this kernel, so 0o755 is used directly.
const DEFAULT_SOCKET_PERMS: u16 = S_IFSOCK | 0o755;

/// Maximum target length for fast (inline) symlinks.
/// Stored in direct_blocks[12] + indirect + double_indirect + triple_indirect = 60 bytes.
pub(crate) const FAST_SYMLINK_MAX: usize = 60;

/// Directory entry file type constants
const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;
/// `EXT2_FT_SOCK`. Recorded in the directory entry alongside the inode's type
/// bits so `getdents64` reports `DT_SOCK` without having to read the inode.
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

/// Minimum directory entry size (inode + rec_len + name_len + file_type)
pub(crate) const DIR_ENTRY_HEADER_SIZE: usize = 8;

// ============================================================================
// On-disk Structures
// ============================================================================
//
// Layout is pinned twice, on purpose (docs/archive/AKUMA_EXT2_CLEANUP.md §2.2):
//
// 1. `repr(C)` + the `offset_of!`/`size_of` assertions below state the
//    on-disk layout the ext2 spec defines, and a wrong field order or width is
//    a compile error instead of a silent misparse of a real filesystem. The
//    structs were `packed` while code reinterpreted them as raw bytes; the
//    codec below made that unnecessary, and `repr(C)` keeps every asserted
//    offset (all fields land naturally aligned, which the size assertions
//    prove — any accidental padding changes the size and fails the build).
// 2. The `parse`/`serialize` impls under "On-disk layout codec" restate each
//    offset as an explicit little-endian read/write, bounds-checked and
//    host-endian-independent. Round-trip tests in `src/tests.rs` prove the two
//    statements agree; a misplaced offset fails a test instead of a live disk.
//
// Nothing reinterprets these structs as bytes any more — every read from a
// device buffer goes through `parse` and every write through `serialize`.

/// Ext2 Superblock (located at byte offset 1024)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Superblock {
    pub(crate) total_inodes: u32,
    pub(crate) total_blocks: u32,
    pub(crate) superuser_blocks: u32,
    pub(crate) unallocated_blocks: u32,
    pub(crate) unallocated_inodes: u32,
    pub(crate) first_data_block: u32,
    pub(crate) block_size_log: u32,
    pub(crate) fragment_size_log: u32,
    pub(crate) blocks_per_group: u32,
    pub(crate) fragments_per_group: u32,
    pub(crate) inodes_per_group: u32,
    pub(crate) last_mount_time: u32,
    pub(crate) last_written_time: u32,
    pub(crate) mount_count: u16,
    pub(crate) max_mount_count: u16,
    pub(crate) magic: u16,
    pub(crate) fs_state: u16,
    pub(crate) error_handling: u16,
    pub(crate) version_minor: u16,
    pub(crate) last_check_time: u32,
    pub(crate) check_interval: u32,
    pub(crate) creator_os: u32,
    pub(crate) version_major: u32,
    pub(crate) reserved_uid: u16,
    pub(crate) reserved_gid: u16,
    pub(crate) first_inode: u32,
    pub(crate) inode_size: u16,
    pub(crate) block_group: u16,
    pub(crate) feature_compat: u32,
    pub(crate) feature_incompat: u32,
    pub(crate) feature_ro_compat: u32,
    pub(crate) uuid: [u8; 16],
    pub(crate) volume_name: [u8; 16],
    pub(crate) last_mounted: [u8; 64],
    pub(crate) algo_bitmap: u32,
    pub(crate) _padding: [u8; 820],
}

/// Block Group Descriptor
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BlockGroupDescriptor {
    pub(crate) block_bitmap: u32,
    pub(crate) inode_bitmap: u32,
    pub(crate) inode_table: u32,
    pub(crate) free_blocks_count: u16,
    pub(crate) free_inodes_count: u16,
    pub(crate) used_dirs_count: u16,
    pub(crate) _padding: u16,
    pub(crate) _reserved: [u8; 12],
}

/// Inode structure
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Inode {
    pub(crate) type_perms: u16,
    pub(crate) uid: u16,
    pub(crate) size_lower: u32,
    pub(crate) access_time: u32,
    pub(crate) creation_time: u32,
    pub(crate) modification_time: u32,
    pub(crate) deletion_time: u32,
    pub(crate) gid: u16,
    pub(crate) hard_links: u16,
    pub(crate) sectors_used: u32,
    pub(crate) flags: u32,
    pub(crate) os_specific_1: u32,
    pub(crate) direct_blocks: [u32; 12],
    pub(crate) indirect_block: u32,
    pub(crate) double_indirect_block: u32,
    pub(crate) triple_indirect_block: u32,
    pub(crate) generation: u32,
    pub(crate) file_acl: u32,
    pub(crate) size_upper: u32,
    pub(crate) fragment_addr: u32,
    pub(crate) os_specific_2: [u8; 12],
}

impl Default for Inode {
    fn default() -> Self {
        Self {
            type_perms: 0,
            uid: 0,
            size_lower: 0,
            access_time: 0,
            creation_time: 0,
            modification_time: 0,
            deletion_time: 0,
            gid: 0,
            hard_links: 0,
            sectors_used: 0,
            flags: 0,
            os_specific_1: 0,
            direct_blocks: [0; 12],
            indirect_block: 0,
            double_indirect_block: 0,
            triple_indirect_block: 0,
            generation: 0,
            file_acl: 0,
            size_upper: 0,
            fragment_addr: 0,
            os_specific_2: [0; 12],
        }
    }
}

/// Directory entry (variable size on disk)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DirEntryRaw {
    pub(crate) inode: u32,
    pub(crate) rec_len: u16,
    pub(crate) name_len: u8,
    pub(crate) file_type: u8,
}

// Layout assertions — see the section header above. The offsets are the ext2
// spec positions; the codec's explicit offsets must agree with these, which is
// what the round-trip tests prove.
const _: () = assert!(size_of::<Superblock>() == 1024);
const _: () = assert!(offset_of!(Superblock, magic) == 56);
const _: () = assert!(offset_of!(Superblock, version_major) == 76);
const _: () = assert!(offset_of!(Superblock, inode_size) == 88);
const _: () = assert!(offset_of!(Superblock, first_data_block) == 20);
const _: () = assert!(offset_of!(Superblock, block_size_log) == 24);
const _: () = assert!(offset_of!(Superblock, blocks_per_group) == 32);
const _: () = assert!(offset_of!(Superblock, inodes_per_group) == 40);
const _: () = assert!(offset_of!(Superblock, total_blocks) == 4);
const _: () = assert!(offset_of!(Superblock, total_inodes) == 0);
const _: () = assert!(offset_of!(Superblock, unallocated_blocks) == 12);
const _: () = assert!(offset_of!(Superblock, unallocated_inodes) == 16);

const _: () = assert!(size_of::<BlockGroupDescriptor>() == 32);
const _: () = assert!(offset_of!(BlockGroupDescriptor, block_bitmap) == 0);
const _: () = assert!(offset_of!(BlockGroupDescriptor, inode_bitmap) == 4);
const _: () = assert!(offset_of!(BlockGroupDescriptor, inode_table) == 8);
const _: () = assert!(offset_of!(BlockGroupDescriptor, free_blocks_count) == 12);
const _: () = assert!(offset_of!(BlockGroupDescriptor, free_inodes_count) == 14);
const _: () = assert!(offset_of!(BlockGroupDescriptor, used_dirs_count) == 16);

const _: () = assert!(size_of::<Inode>() == 128);
const _: () = assert!(offset_of!(Inode, type_perms) == 0);
const _: () = assert!(offset_of!(Inode, uid) == 2);
const _: () = assert!(offset_of!(Inode, size_lower) == 4);
const _: () = assert!(offset_of!(Inode, gid) == 24);
const _: () = assert!(offset_of!(Inode, hard_links) == 26);
const _: () = assert!(offset_of!(Inode, sectors_used) == 28);
const _: () = assert!(offset_of!(Inode, direct_blocks) == 40);
const _: () = assert!(offset_of!(Inode, indirect_block) == 88);
const _: () = assert!(offset_of!(Inode, double_indirect_block) == 92);
const _: () = assert!(offset_of!(Inode, triple_indirect_block) == 96);
const _: () = assert!(offset_of!(Inode, generation) == 100);
const _: () = assert!(offset_of!(Inode, file_acl) == 104);
const _: () = assert!(offset_of!(Inode, size_upper) == 108);
const _: () = assert!(offset_of!(Inode, fragment_addr) == 112);

const _: () = assert!(size_of::<DirEntryRaw>() == DIR_ENTRY_HEADER_SIZE);
const _: () = assert!(offset_of!(DirEntryRaw, inode) == 0);
const _: () = assert!(offset_of!(DirEntryRaw, rec_len) == 4);
const _: () = assert!(offset_of!(DirEntryRaw, name_len) == 6);
const _: () = assert!(offset_of!(DirEntryRaw, file_type) == 7);

// ============================================================================
// On-disk layout codec
// ============================================================================
//
// Explicit offset-based parse/serialize for the four on-disk structures
// (docs/archive/AKUMA_EXT2_CLEANUP.md §2.2). This replaces the old
// `read_unaligned`/`from_raw_parts` blits: every offset below is stated twice
// by hand (read and write), is checkable against the ext2 spec by eye, and is
// bounds-checked — a short or corrupt buffer yields `None`, never an out-of-
// bounds read. All multi-byte fields are `from_le_bytes`/`to_le_bytes`, so the
// format's little-endianness is explicit instead of borrowing the host's.
//
// The padding/reserved fields are carried verbatim (not interpreted) so a
// write-back of a parsed structure is byte-faithful to what was read; ext2
// reserves those bytes and e2fsck notices when drivers scribble on them.

/// Little-endian `u16` at byte offset `at`, or `None` past the end of `buf`.
fn le_u16(buf: &[u8], at: usize) -> Option<u16> {
    let b: [u8; 2] = buf.get(at..at + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(b))
}

/// Little-endian `u32` at byte offset `at`, or `None` past the end of `buf`.
fn le_u32(buf: &[u8], at: usize) -> Option<u32> {
    let b: [u8; 4] = buf.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(b))
}

fn put_u16(out: &mut [u8], at: usize, v: u16) {
    out[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut [u8], at: usize, v: u32) {
    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

impl Superblock {
    pub(crate) const SIZE: usize = size_of::<Self>();

    /// Parse the 1024-byte superblock image read from [`SUPERBLOCK_OFFSET`].
    /// `None` on a short buffer — the caller has already checked the magic by
    /// the time validation matters, and every disk-supplied *arithmetic* input
    /// is validated at the mount path (see `Ext2Filesystem::new_with_cache_cap`).
    pub(crate) fn parse(buf: &[u8]) -> Option<Self> {
        Some(Self {
            total_inodes: le_u32(buf, 0)?,
            total_blocks: le_u32(buf, 4)?,
            superuser_blocks: le_u32(buf, 8)?,
            unallocated_blocks: le_u32(buf, 12)?,
            unallocated_inodes: le_u32(buf, 16)?,
            first_data_block: le_u32(buf, 20)?,
            block_size_log: le_u32(buf, 24)?,
            fragment_size_log: le_u32(buf, 28)?,
            blocks_per_group: le_u32(buf, 32)?,
            fragments_per_group: le_u32(buf, 36)?,
            inodes_per_group: le_u32(buf, 40)?,
            last_mount_time: le_u32(buf, 44)?,
            last_written_time: le_u32(buf, 48)?,
            mount_count: le_u16(buf, 52)?,
            max_mount_count: le_u16(buf, 54)?,
            magic: le_u16(buf, 56)?,
            fs_state: le_u16(buf, 58)?,
            error_handling: le_u16(buf, 60)?,
            version_minor: le_u16(buf, 62)?,
            last_check_time: le_u32(buf, 64)?,
            check_interval: le_u32(buf, 68)?,
            creator_os: le_u32(buf, 72)?,
            version_major: le_u32(buf, 76)?,
            reserved_uid: le_u16(buf, 80)?,
            reserved_gid: le_u16(buf, 82)?,
            first_inode: le_u32(buf, 84)?,
            inode_size: le_u16(buf, 88)?,
            block_group: le_u16(buf, 90)?,
            feature_compat: le_u32(buf, 92)?,
            feature_incompat: le_u32(buf, 96)?,
            feature_ro_compat: le_u32(buf, 100)?,
            uuid: buf.get(104..120)?.try_into().ok()?,
            volume_name: buf.get(120..136)?.try_into().ok()?,
            last_mounted: buf.get(136..200)?.try_into().ok()?,
            algo_bitmap: le_u32(buf, 200)?,
            // Bytes 204..1024: reserved. Carried verbatim so a write-back is
            // byte-identical to the image we mounted.
            _padding: buf.get(204..Self::SIZE)?.try_into().ok()?,
        })
    }

    /// Serialize to the exact-size superblock image (1024 bytes). `out` shorter
    /// than that panics — callers pass a `[u8; 1024]`.
    pub(crate) fn serialize(&self, out: &mut [u8]) {
        put_u32(out, 0, self.total_inodes);
        put_u32(out, 4, self.total_blocks);
        put_u32(out, 8, self.superuser_blocks);
        put_u32(out, 12, self.unallocated_blocks);
        put_u32(out, 16, self.unallocated_inodes);
        put_u32(out, 20, self.first_data_block);
        put_u32(out, 24, self.block_size_log);
        put_u32(out, 28, self.fragment_size_log);
        put_u32(out, 32, self.blocks_per_group);
        put_u32(out, 36, self.fragments_per_group);
        put_u32(out, 40, self.inodes_per_group);
        put_u32(out, 44, self.last_mount_time);
        put_u32(out, 48, self.last_written_time);
        put_u16(out, 52, self.mount_count);
        put_u16(out, 54, self.max_mount_count);
        put_u16(out, 56, self.magic);
        put_u16(out, 58, self.fs_state);
        put_u16(out, 60, self.error_handling);
        put_u16(out, 62, self.version_minor);
        put_u32(out, 64, self.last_check_time);
        put_u32(out, 68, self.check_interval);
        put_u32(out, 72, self.creator_os);
        put_u32(out, 76, self.version_major);
        put_u16(out, 80, self.reserved_uid);
        put_u16(out, 82, self.reserved_gid);
        put_u32(out, 84, self.first_inode);
        put_u16(out, 88, self.inode_size);
        put_u16(out, 90, self.block_group);
        put_u32(out, 92, self.feature_compat);
        put_u32(out, 96, self.feature_incompat);
        put_u32(out, 100, self.feature_ro_compat);
        out[104..120].copy_from_slice(&self.uuid);
        out[120..136].copy_from_slice(&self.volume_name);
        out[136..200].copy_from_slice(&self.last_mounted);
        put_u32(out, 200, self.algo_bitmap);
        out[204..Self::SIZE].copy_from_slice(&self._padding);
    }
}

impl BlockGroupDescriptor {
    pub(crate) const SIZE: usize = size_of::<Self>();

    /// Parse one 32-byte block-group descriptor out of the BGD table block.
    pub(crate) fn parse(buf: &[u8]) -> Option<Self> {
        Some(Self {
            block_bitmap: le_u32(buf, 0)?,
            inode_bitmap: le_u32(buf, 4)?,
            inode_table: le_u32(buf, 8)?,
            free_blocks_count: le_u16(buf, 12)?,
            free_inodes_count: le_u16(buf, 14)?,
            used_dirs_count: le_u16(buf, 16)?,
            _padding: le_u16(buf, 18)?,
            _reserved: buf.get(20..Self::SIZE)?.try_into().ok()?,
        })
    }

    /// Serialize to the exact-size 32-byte descriptor image.
    pub(crate) fn serialize(&self, out: &mut [u8]) {
        put_u32(out, 0, self.block_bitmap);
        put_u32(out, 4, self.inode_bitmap);
        put_u32(out, 8, self.inode_table);
        put_u16(out, 12, self.free_blocks_count);
        put_u16(out, 14, self.free_inodes_count);
        put_u16(out, 16, self.used_dirs_count);
        put_u16(out, 18, self._padding);
        out[20..Self::SIZE].copy_from_slice(&self._reserved);
    }
}

/// The 15 block-pointer words of an inode (`direct_blocks[0..12]` plus the
/// three indirection pointers): bytes 40..100 of the serialized form. A fast
/// symlink's target lives in exactly this window — Linux's convention, so a
/// target written here is readable by Linux and e2fsck unchanged
/// (docs/archive/AKUMA_EXT2_CLEANUP.md §3).
pub(crate) const INODE_POINTER_WORDS: usize = 15;
pub(crate) const INODE_POINTERS_OFFSET: usize = offset_of!(Inode, direct_blocks);

impl Inode {
    pub(crate) const SIZE: usize = size_of::<Self>();

    /// The `i`-th pointer word (0..[`INODE_POINTER_WORDS`]) as a plain field
    /// access. This is what lets the fast-symlink window be read and written
    /// byte-exactly without ever taking a reference into a packed struct or
    /// reinterpreting the inode as bytes.
    fn pointer_word(&self, i: usize) -> u32 {
        debug_assert!(i < INODE_POINTER_WORDS);
        match i {
            0..=11 => self.direct_blocks[i],
            12 => self.indirect_block,
            13 => self.double_indirect_block,
            14 => self.triple_indirect_block,
            _ => 0,
        }
    }

    /// Mutable form of [`Self::pointer_word`].
    fn pointer_word_mut(&mut self, i: usize) -> &mut u32 {
        debug_assert!(i < INODE_POINTER_WORDS);
        match i {
            0..=11 => &mut self.direct_blocks[i],
            12 => &mut self.indirect_block,
            13 => &mut self.double_indirect_block,
            _ => &mut self.triple_indirect_block,
        }
    }

    /// Store a fast-symlink target in the pointer-word window, zeroing it
    /// first. `target` longer than [`FAST_SYMLINK_MAX`] is truncated by the
    /// caller (the fast/slow split happens before this is reached).
    pub(crate) fn set_fast_symlink_target(&mut self, target: &[u8]) {
        for w in 0..INODE_POINTER_WORDS {
            *self.pointer_word_mut(w) = 0;
        }
        for (i, &b) in target.iter().take(FAST_SYMLINK_MAX).enumerate() {
            *self.pointer_word_mut(i / 4) |= u32::from(b) << (8 * (i % 4));
        }
    }

    /// Read back a fast-symlink target of `len` bytes (≤ [`FAST_SYMLINK_MAX`])
    /// from the pointer-word window, little-endian — the exact inverse of
    /// [`Self::set_fast_symlink_target`], and the exact bytes 40..100 of the
    /// serialized inode.
    pub(crate) fn fast_symlink_target(&self, len: usize) -> [u8; FAST_SYMLINK_MAX] {
        let mut out = [0u8; FAST_SYMLINK_MAX];
        for (i, b) in out.iter_mut().take(len).enumerate() {
            *b = (self.pointer_word(i / 4) >> (8 * (i % 4))) as u8;
        }
        out
    }

    /// Parse one [`Self::SIZE`]-byte inode-table entry. `buf` may be longer
    /// (rev-1 filesystems with larger on-disk inodes): only the first 128 bytes
    /// are read, and bounds-checking makes a short buffer `None` instead of an
    /// over-read — the mount path separately rejects `inode_size < 128`.
    pub(crate) fn parse(buf: &[u8]) -> Option<Self> {
        let mut direct_blocks = [0u32; 12];
        for (i, w) in direct_blocks.iter_mut().enumerate() {
            *w = le_u32(buf, INODE_POINTERS_OFFSET + i * 4)?;
        }
        Some(Self {
            type_perms: le_u16(buf, 0)?,
            uid: le_u16(buf, 2)?,
            size_lower: le_u32(buf, 4)?,
            access_time: le_u32(buf, 8)?,
            creation_time: le_u32(buf, 12)?,
            modification_time: le_u32(buf, 16)?,
            deletion_time: le_u32(buf, 20)?,
            gid: le_u16(buf, 24)?,
            hard_links: le_u16(buf, 26)?,
            sectors_used: le_u32(buf, 28)?,
            flags: le_u32(buf, 32)?,
            os_specific_1: le_u32(buf, 36)?,
            direct_blocks,
            indirect_block: le_u32(buf, 88)?,
            double_indirect_block: le_u32(buf, 92)?,
            triple_indirect_block: le_u32(buf, 96)?,
            generation: le_u32(buf, 100)?,
            file_acl: le_u32(buf, 104)?,
            size_upper: le_u32(buf, 108)?,
            fragment_addr: le_u32(buf, 112)?,
            os_specific_2: buf.get(116..Self::SIZE)?.try_into().ok()?,
        })
    }

    /// Serialize to the exact-size 128-byte inode-table entry.
    pub(crate) fn serialize(&self, out: &mut [u8]) {
        put_u16(out, 0, self.type_perms);
        put_u16(out, 2, self.uid);
        put_u32(out, 4, self.size_lower);
        put_u32(out, 8, self.access_time);
        put_u32(out, 12, self.creation_time);
        put_u32(out, 16, self.modification_time);
        put_u32(out, 20, self.deletion_time);
        put_u16(out, 24, self.gid);
        put_u16(out, 26, self.hard_links);
        put_u32(out, 28, self.sectors_used);
        put_u32(out, 32, self.flags);
        put_u32(out, 36, self.os_specific_1);
        for (i, w) in self.direct_blocks.iter().enumerate() {
            put_u32(out, INODE_POINTERS_OFFSET + i * 4, *w);
        }
        put_u32(out, 88, self.indirect_block);
        put_u32(out, 92, self.double_indirect_block);
        put_u32(out, 96, self.triple_indirect_block);
        put_u32(out, 100, self.generation);
        put_u32(out, 104, self.file_acl);
        put_u32(out, 108, self.size_upper);
        put_u32(out, 112, self.fragment_addr);
        out[116..Self::SIZE].copy_from_slice(&self.os_specific_2);
    }
}

impl DirEntryRaw {
    /// Parse the fixed 8-byte header at the start of `buf` (callers pass the
    /// directory slice from the entry's own offset). `None` on a short tail —
    /// every caller's loop guard already promises 8 bytes, so this is belt
    /// and braces, not the primary defense.
    pub(crate) fn parse(buf: &[u8]) -> Option<Self> {
        Some(Self {
            inode: le_u32(buf, 0)?,
            rec_len: le_u16(buf, 4)?,
            name_len: *buf.get(6)?,
            file_type: *buf.get(7)?,
        })
    }

    /// Serialize the 8-byte header into `out` at its start. The entry's
    /// `rec_len` padding (a real entry's `rec_len` may exceed
    /// header + name length) lives outside the header and is untouched here.
    pub(crate) fn serialize(&self, out: &mut [u8]) {
        put_u32(out, 0, self.inode);
        put_u16(out, 4, self.rec_len);
        out[6] = self.name_len;
        out[7] = self.file_type;
    }
}

// ============================================================================
// Ext2 Filesystem State
// ============================================================================

/// Internal filesystem state. `pub(crate)` so `mod tests` can reach `fs.state`;
/// nothing outside the crate names it.
pub(crate) struct Ext2State {
    superblock: Superblock,
    block_size: usize,
    inodes_per_group: u32,
    inode_size: u16,
    block_group_count: u32,
    blocks_per_group: u32,
    first_data_block: u32,

    // ---- deferred metadata writeback (see `flush_meta`) ----
    //
    // The superblock free counts and every block-group descriptor's free counts
    // are updated on *every* block/inode allocate and free. Writing them through
    // per operation cost a full 1 KB superblock write + a BGD-block RMW per
    // allocated block — ~1000 device writes for a 2 MB file (see
    // `crates/akuma-ext2/README.md` § Performance). These fields hold the
    // authoritative in-memory copy; `flush_meta` writes the dirty ones to disk
    // at the end of every mutating `Filesystem` method and on `sync()`. Only the
    // write-locked allocator paths touch them, so a plain `Vec` is safe (no
    // concurrent readers of *these* fields — read-only paths that call
    // `read_bgd` only ever read `bgd.inode_table`, which never changes).
    /// `bgd_cache[g]` = the current BGD for group `g`, `None` until first read.
    bgd_cache: Vec<Option<BlockGroupDescriptor>>,
    /// `bgd_dirty[g]` = group `g`'s BGD differs from disk.
    bgd_dirty: Vec<bool>,
    /// The in-memory superblock is ahead of disk.
    sb_dirty: bool,
    /// Allocation bitmap blocks (block-bitmap / inode-bitmap) touched by the
    /// allocators, `(physical block number, contents, dirty)`. Same rationale as
    /// `bgd_cache`: a 2 MB write set + cleared ~512 bits in one block, each a
    /// full-block RMW device write. Only the four allocators touch a bitmap
    /// block, and only through [`Self::bitmap_slot`], so this is the single
    /// source of truth for them; `flush_meta` writes the dirty ones. Bounded by
    /// `2 * block_group_count` entries, populated lazily.
    bitmap_cache: Vec<(u32, Vec<u8>, bool)>,
    /// Next-free-bit scan cursor per group for `allocate_block_inner`
    /// (design doc D-6). Sequential allocation used to rescan the bitmap from
    /// bit 0 on every call — O(N²) in blocks allocated. The cursor is only a
    /// hint (a miss wraps and rescans from 0), so it can never skip a free
    /// bit; frees pull it back so deleted-file space is reused immediately.
    block_hint: Vec<u32>,
    /// Same cursor for `allocate_inode` / `free_inode`.
    inode_hint: Vec<u32>,
}

/// Non-preemption/IRQ guard held for the lifetime of an [`Ext2State`] lock hold under
/// `no-bkl-vfs`; a zero-sized no-op otherwise.
///
/// Every ext2 lock hold does real block I/O (see [`Ext2Filesystem::read_block`]), so it
/// must never be descheduled mid-hold: the Big Kernel Lock is released on an EL1→EL0
/// return, and a stranded `state` guard means any other core that then spins for it does
/// so *while holding the BKL* — the holder can never be rescheduled to release it, and
/// every core piles into the BKL wait. With `no-bkl-vfs` the same guard additionally
/// masks local IRQs, so a core inside an ext2 critical section without the BKL can't take
/// a nested IRQ whose `enter_kernel()` hard-spins for the BKL while this core holds the
/// inner lock (AB-BA — the SMP=4 wedge `no-bkl-network` closed the same way).
///
/// **IRQs are masked around the non-blocking `try_*` and the resulting hold only, never
/// across the acquisition backoff** — see [`Ext2Filesystem::read_state`].
#[cfg(feature = "no-bkl-vfs")]
type StateHoldGuard = akuma_primitives::PreemptGuard;
#[cfg(not(feature = "no-bkl-vfs"))]
type StateHoldGuard = NoStateHold;

/// The `StateHoldGuard` stand-in when `no-bkl-vfs` is off: a ZST that compiles to nothing.
///
/// Deliberately **not** `Copy`, and carrying an empty `Drop`, so that the explicit
/// `drop(hold)` before each backoff spin type-checks identically in both configurations —
/// without it, the same source line is a real release in one build and a lint
/// (`dropping_copy_types` / `drop_non_drop`) in the other.
#[cfg(not(feature = "no-bkl-vfs"))]
struct NoStateHold;

#[cfg(not(feature = "no-bkl-vfs"))]
impl Drop for NoStateHold {
    fn drop(&mut self) {}
}

/// Take a [`StateHoldGuard`] for one `try_*` attempt (and, on success, the hold).
#[inline]
fn state_hold_guard() -> StateHoldGuard {
    #[cfg(feature = "no-bkl-vfs")]
    return akuma_primitives::PreemptGuard::new();
    #[cfg(not(feature = "no-bkl-vfs"))]
    NoStateHold
}

/// RAII guard for read access to `Ext2State` (no ownership tracking needed — reads are
/// concurrent). Carries the [`StateHoldGuard`] so preemption/IRQ state is restored
/// exactly when the lock is released.
struct Ext2ReadGuard<'a> {
    // Declaration order IS drop order: release the lock first, THEN restore
    // preemption/IRQs — never the other way round, or the window this guard exists to
    // close would reopen for the instant between them.
    inner: akuma_locks_rw_cell::ReadGuard<'a, Ext2State>,
    #[allow(dead_code)]
    hold: StateHoldGuard,
}

impl core::ops::Deref for Ext2ReadGuard<'_> {
    type Target = Ext2State;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// RAII guard for write access to `Ext2State`.
///
/// The explicit owner-clearing `Drop` this used to carry is gone: the owner
/// cell belongs to the lock now, and `WriteTicket`'s own `Drop` clears it
/// before the writer bit falls — the same ordering, one layer down and
/// model-checked there.
struct Ext2WriteGuard<'a> {
    // See `Ext2ReadGuard` for why `inner` must precede `hold`.
    inner: akuma_locks_rw_cell::WriteGuard<'a, Ext2State>,
    #[allow(dead_code)]
    hold: StateHoldGuard,
}

impl core::ops::Deref for Ext2WriteGuard<'_> {
    type Target = Ext2State;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for Ext2WriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// ============================================================================
// Ext2 Filesystem Implementation
// ============================================================================

/// Ext2 filesystem implementation
pub struct Ext2Filesystem<B: BlockDevice> {
    dev: B,
    time_fn: fn() -> u64,
    /// Internal state behind the recoverable reader/writer lock. Reads proceed
    /// concurrently; a holder killed mid-hold is recovered by
    /// [`Self::abandon_tid`] rather than by this crate inferring liveness.
    /// `pub(crate)` for `mod tests`; not reachable outside the crate.
    pub(crate) state: RecoverableCell<Ext2State>,
    #[cfg(not(kernel_profile_extreme))]
    block_cache: Spinlock<BlockCache>,
    /// Inodes unlinked while a mapping still pinned them — see [`DeferredFrees`].
    deferred: DeferredFrees,
    /// Directory-tree walks and block-cache accesses this instance has done, for
    /// [`Self::work_counters`].
    ///
    /// **Per-instance, and that is the point**: the crate's other counters
    /// (`CACHE_HITS`, `E2_READ_AT_EOF`) are global statics, so a test asserting
    /// on a delta across them races every other test `cargo test` runs in
    /// parallel. These belong to one mount and answer for one test's work only.
    #[cfg(test)]
    counters: WorkCounters,
}

/// Deterministic work counters for one filesystem instance — see
/// [`Ext2Filesystem::work_counters`]. Test-only: nothing outside `cargo test`
/// reads them, and the increments must not exist on the hot path in a shipped
/// kernel.
#[cfg(test)]
#[derive(Default)]
struct WorkCounters {
    /// Calls to `lookup_path_internal`, i.e. full directory-tree walks.
    path_walks: core::sync::atomic::AtomicU64,
    /// Calls to `with_block`, i.e. block-cache accesses (hit or miss).
    block_accesses: core::sync::atomic::AtomicU64,
}

/// The active block cache type: the large clock cache under the `fs-cache`
/// feature, otherwise the tiny 64-slot ring. (`extreme` has neither.)
#[cfg(all(not(kernel_profile_extreme), ext2_fs_cache))]
type BlockCache = ClockBlockCache;
#[cfg(all(not(kernel_profile_extreme), not(ext2_fs_cache)))]
type BlockCache = BlockRingCache;

/// Build the block cache for a new filesystem instance. `cap` is the
/// per-instance override from [`Ext2Filesystem::new_with_cache_cap`]; `None`
/// sizes from the crate-global cap as before.
#[cfg(all(not(kernel_profile_extreme), ext2_fs_cache))]
fn make_block_cache(block_size: usize, cap: Option<usize>) -> BlockCache {
    match cap {
        Some(bytes) => {
            let slots = core::cmp::max(64, bytes / block_size.max(1));
            ClockBlockCache::with_capacity_blocks(block_size, slots)
        }
        None => ClockBlockCache::new(block_size),
    }
}

#[cfg(all(not(kernel_profile_extreme), not(ext2_fs_cache)))]
fn make_block_cache(block_size: usize, _cap: Option<usize>) -> BlockCache {
    BlockRingCache::new(block_size)
}

impl<B: BlockDevice> Ext2Filesystem<B> {
    /// Create a new Ext2 filesystem backed by `dev`, with the block cache
    /// sized from the crate-global cap (`set_cache_cap_bytes`).
    ///
    /// `utc_time_us` returns the current UTC time in microseconds since epoch;
    /// pass `|| 0` if timestamps are not needed.
    pub fn new(dev: B, utc_time_us: fn() -> u64) -> Result<Self, FsError> {
        Self::new_with_cache_cap(dev, utc_time_us, None)
    }

    /// [`Self::new`] with an explicit block-cache cap in bytes, for the
    /// *second* and later ext2 instances a runtime `mount(2)` creates: the
    /// global cap is a budget for the root filesystem alone, and applying it
    /// per-instance again would double the committed heap (the cache never
    /// shrinks — see `src/fs.rs`). `None` means "use the global cap".
    /// Ignored on profiles without the large cache (`extreme`, no `fs-cache`).
    #[allow(unused_variables)]
    pub fn new_with_cache_cap(
        dev: B,
        utc_time_us: fn() -> u64,
        cache_cap_bytes: Option<usize>,
    ) -> Result<Self, FsError> {
        let mut sb_buf = [0u8; 1024];
        dev.read_bytes(SUPERBLOCK_OFFSET, &mut sb_buf).map_err(|_| FsError::IoError)?;

        let superblock = Superblock::parse(&sb_buf).ok_or(FsError::Corrupt)?;

        let magic = superblock.magic;
        let block_size_log = superblock.block_size_log;
        let version_major = superblock.version_major;
        let sb_inode_size = superblock.inode_size;
        let total_blocks = superblock.total_blocks;
        let _total_inodes = superblock.total_inodes;
        let blocks_per_group = superblock.blocks_per_group;
        let inodes_per_group = superblock.inodes_per_group;

        if magic != EXT2_MAGIC {
            return Err(FsError::NoFilesystem);
        }

        // ── On-disk arithmetic validation (AKUMA_EXT2_CLEANUP.md §2.3) ──────
        //
        // Everything below feeds a division, a shift, or a heap allocation
        // length further down. A corrupt-but-magic-matching image used to be
        // able to panic at mount (÷0, shift overflow) or read past a heap
        // allocation (`read_inode` blitting 128 bytes out of an
        // `inode_size`-byte buffer). A filesystem driver must survive a
        // corrupted disk image, so reject it here instead.
        //
        // Max block size 64 KiB: `block_size_log` 0..=6 is the range Linux
        // accepts (mke2fs -b 1024..65536); anything above would wrap the
        // shift into a garbage block size.
        if block_size_log > 6 {
            return Err(FsError::Corrupt);
        }
        if blocks_per_group == 0 || inodes_per_group == 0 {
            // Both divide: block_group_count below, and inode-table indexing
            // (`inode_idx / inodes_per_group`) on every inode read.
            return Err(FsError::Corrupt);
        }
        if total_blocks < superblock.first_data_block {
            // `block_group_count` subtracts these; the underflow used to wrap
            // into a garbage group count on a corrupt image.
            return Err(FsError::Corrupt);
        }

        let block_size = 1024usize << block_size_log;
        let inode_size = if version_major >= 1 {
            sb_inode_size
        } else {
            // Revision 0 has no inode_size field; the entry is 128 bytes.
            128
        };
        // A whole `Inode` is read out of each `inode_size`-byte table entry —
        // anything smaller is the heap over-read §2.3 documents.
        if (inode_size as usize) < size_of::<Inode>() {
            return Err(FsError::Corrupt);
        }
        let first_data_block = superblock.first_data_block;
        let block_group_count =
            (total_blocks - first_data_block + blocks_per_group - 1) / blocks_per_group;

        let state = Ext2State {
            superblock,
            block_size,
            inodes_per_group,
            inode_size,
            block_group_count,
            blocks_per_group,
            first_data_block,
            bgd_cache: vec![None; block_group_count as usize],
            bgd_dirty: vec![false; block_group_count as usize],
            sb_dirty: false,
            bitmap_cache: Vec::new(),
            block_hint: vec![0; block_group_count as usize],
            inode_hint: vec![0; block_group_count as usize],
        };

        Ok(Self {
            dev,
            time_fn: utc_time_us,
            state: RecoverableCell::new(state),
            #[cfg(not(kernel_profile_extreme))]
            block_cache: Spinlock::new(make_block_cache(block_size, cache_cap_bytes)),
            deferred: DeferredFrees::new(),
            #[cfg(test)]
            counters: WorkCounters::default(),
        })
    }

    fn current_time(&self) -> u32 {
        ((self.time_fn)() / 1_000_000) as u32
    }

    /// Try to acquire a read lock with a retry limit (used by unit tests only).
    /// Returns None if it cannot be acquired within `max_retries` attempts.
    #[cfg(test)]
    pub fn try_lock_state(&self, max_retries: u32) -> Option<impl core::ops::Deref<Target = Ext2State> + '_> {
        for _ in 0..max_retries {
            let hold = state_hold_guard();
            if let Some(inner) = self.state.try_read() {
                return Some(Ext2ReadGuard { inner, hold });
            }
            drop(hold);
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
        None
    }

    /// Acquire a read lock, blocking until available. Reads are concurrent.
    ///
    /// The [`StateHoldGuard`] is taken *per attempt*, immediately before the
    /// non-blocking try, and either kept (success — it now covers the hold) or
    /// dropped before the backoff spin. It deliberately does **not** wrap the whole
    /// wait: under `no-bkl-vfs` that guard masks local IRQs, and this wait is
    /// unbounded, so masking across it would starve this core's timer for the whole
    /// contended window — and if the current holder were a thread on this core,
    /// nothing could ever run to release it. Masking only the try + hold gives the
    /// property we actually need (no nested exception *while holding* the lock)
    /// with a bounded masked window.
    ///
    /// That discipline is why this calls `read_holding` rather than a plain
    /// `read()`: the loop lives in `akuma-locks-rw`, which takes the guard on the
    /// caller's behalf at exactly those two points. Before 2026-08-31 this
    /// function *was* the loop, in one of three drifting copies, each carrying its
    /// own 10 000-spin orphan-recovery branch that asked the scheduler whether the
    /// recorded owner tid was dead and then `force_unlock_write()`-ed on the
    /// answer. All of that is gone — `AKUMA_EXT2_CLEANUP.md` §4.2a for why the
    /// question was unanswerable (a recycled tid made it read a *new* occupant's
    /// liveness), §4.3a for what replaced it.
    #[allow(dead_code)]
    fn read_state(&self) -> Ext2ReadGuard<'_> {
        let (inner, hold) = self.state.read_holding(state_hold_guard);
        Ext2ReadGuard { inner, hold }
    }

    /// Acquire a write lock, blocking until available. Same per-attempt
    /// [`StateHoldGuard`] discipline as [`Self::read_state`] — see there.
    fn write_state(&self) -> Ext2WriteGuard<'_> {
        let (inner, hold) = self.state.write_holding(state_hold_guard);
        Ext2WriteGuard { inner, hold }
    }

    /// Recover everything the dead thread `tid` held on this mount.
    ///
    /// The runtime calls this at the TERMINATED→FREE transition, where `tid` is
    /// known dead and its slot cannot yet be reissued. It performs the same
    /// CAS-guarded release a live holder's guard performs, so a sweep that
    /// races a legitimate release, or runs twice, is a no-op — and a lock a
    /// *live* thread holds is never touched. Returns whether anything was
    /// recovered.
    ///
    /// This replaces the three `unsafe { force_unlock_write() }` sites, whose
    /// contract ("no guard for this lock exists") was a whole-program property
    /// no crate could check.
    pub fn abandon_tid(&self, tid: usize) -> bool {
        self.state.abandon_tid(tid)
    }

    // ========================================================================
    // Block I/O
    // ========================================================================

    /// Run `f` over block `block_num`'s bytes **without copying them out**
    /// (design doc D-4). On a cache hit `f` sees the cached slot directly; only
    /// on a miss is a buffer allocated, and then only because the device read
    /// needs somewhere to land.
    ///
    /// Every read path that just copies bytes somewhere — `read_range`,
    /// `get_block_num`'s indirect walks, `read_inode_data`, `read_at_by_inode`
    /// — goes through this. Before D-4 each of those paid a `Vec` allocation
    /// plus a full block memcpy *per hit*, which is pure overhead once
    /// write-back means hits are the common case.
    ///
    /// **`f` must not re-enter the filesystem.** On the hit path it runs with
    /// the `block_cache` lock held, so anything that touches the cache
    /// (`read_block`, `write_block`, `free_block`, `invalidate_block`, or any
    /// `Filesystem` method) deadlocks on the same spinlock. Keep `f` to pure
    /// inspection/copying of the bytes. This is why the `truncate_inode` /
    /// `free_inode_blocks` indirect walks still take the owned
    /// [`Self::read_block`]: they call `free_block` inside the loop.
    ///
    /// Lock-hold time is unchanged from the old code, which also memcpy'd
    /// (`to_vec`) with the lock held — this just drops the allocation.
    fn with_block<R>(
        &self,
        state: &Ext2State,
        block_num: u32,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, FsError> {
        #[cfg(test)]
        self.counters.block_accesses.fetch_add(1, Ordering::Relaxed);
        #[cfg(not(kernel_profile_extreme))]
        {
            {
                let cache = self.block_cache.lock();
                if let Some(data) = cache.get(block_num) {
                    #[cfg(any(ext2_fs_cache, test))]
                    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                    // The E2C coherence oracle needs a snapshot to compare
                    // against the device, and `verify_cached_block` re-locks
                    // the cache — so when it is armed (off by default) fall
                    // back to the old copy-then-verify shape rather than
                    // deadlocking. Zero-copy is the default path.
                    if E2_VERIFY_HITS.load(Ordering::Relaxed) {
                        let snapshot = data.to_vec();
                        drop(cache);
                        self.verify_cached_block(state, block_num, &snapshot);
                        return Ok(f(&snapshot));
                    }
                    return Ok(f(data));
                }
            }
            #[cfg(any(ext2_fs_cache, test))]
            CACHE_MISSES.fetch_add(1, Ordering::Relaxed);

            let mut buf = vec![0u8; state.block_size];
            let offset = block_num as u64 * state.block_size as u64;
            self.dev.read_bytes(offset, &mut buf).map_err(|_| FsError::IoError)?;
            {
                let mut cache = self.block_cache.lock();
                let mut flush = self.dev_flush(state);
                cache.insert(block_num, &buf, &mut flush)?;
            }
            Ok(f(&buf))
        }
        #[cfg(kernel_profile_extreme)]
        {
            let mut buf = vec![0u8; state.block_size];
            let offset = block_num as u64 * state.block_size as u64;
            self.dev.read_bytes(offset, &mut buf).map_err(|_| FsError::IoError)?;
            Ok(f(&buf))
        }
    }

    /// [`Self::with_block`] materialised into an owned `Vec`. For the callers
    /// that genuinely need ownership — they mutate the block and write it back,
    /// or park it in `bitmap_cache` — or that must call back into the fs while
    /// holding the contents. Read-only callers should use `with_block`.
    fn read_block(&self, state: &Ext2State, block_num: u32) -> Result<Vec<u8>, FsError> {
        self.with_block(state, block_num, <[u8]>::to_vec)
    }

    /// The device-write closure cache flushes use — borrows `self.dev`, which
    /// is field-disjoint from `self.block_cache`, so it coexists with a held
    /// cache-lock guard.
    #[cfg(not(kernel_profile_extreme))]
    fn dev_flush<'s>(&'s self, state: &'s Ext2State) -> impl FnMut(u32, &[u8]) -> Result<(), FsError> + 's {
        let bs = state.block_size as u64;
        move |bn: u32, bytes: &[u8]| {
            self.dev.write_bytes(bn as u64 * bs, bytes).map_err(|_| FsError::IoError)
        }
    }

    /// Diagnostic (2026-08-15 zero-page hunt): re-read a block straight from the
    /// device and compare against what the cache served. A mismatch means the
    /// cache is holding bytes the disk no longer has — stale-instrument class
    /// (see `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §13): every earlier
    /// exoneration verified a *different* layer than the one serving these reads.
    /// Bypasses the cache entirely (`self.dev.read_bytes`), so it cannot be
    /// fooled by the entry it is checking. Rate-limited to the first 32 prints;
    /// the counter keeps counting.
    #[cfg(not(kernel_profile_extreme))]
    fn verify_cached_block(&self, state: &Ext2State, block_num: u32, cached: &[u8]) {
        if !E2_VERIFY_HITS.load(Ordering::Relaxed) {
            return;
        }
        // Write-back: a dirty resident block is legitimately ahead of the
        // disk — that is the cache's job now, not staleness.
        if self.block_cache.lock().is_dirty(block_num) {
            return;
        }
        let bs = state.block_size;
        let mut disk = alloc::vec![0u8; bs];
        let off = block_num as u64 * bs as u64;
        if self.dev.read_bytes(off, &mut disk).is_err() {
            return;
        }
        if disk != cached {
            let prev = E2_CACHE_VERIFY_MISMATCH.fetch_add(1, Ordering::Relaxed);
            if prev < 32 {
                let at = disk.iter().zip(cached.iter()).position(|(a, b)| a != b).unwrap_or(0);
                akuma_primitives::safe_print!(224,
                    "[E2C-BAD] block={:#x} first_diff={:#x} cached={:#x} disk={:#x} cached_zero={}\n",
                    block_num, at, cached[at], disk[at], u8::from(cached.iter().all(|b| *b == 0)));
            }
        }
    }

    /// Write a block: mark it dirty in the cache (device write deferred to
    /// `flush_meta`/`sync`/eviction — write-back). `extreme` has no cache and
    /// writes through to the device, preserving its old durability model.
    fn write_block(&self, state: &Ext2State, block_num: u32, data: &[u8]) -> Result<(), FsError> {
        if data.len() != state.block_size {
            return Err(FsError::Internal);
        }
        #[cfg(not(kernel_profile_extreme))]
        {
            let mut cache = self.block_cache.lock();
            let mut flush = self.dev_flush(state);
            cache.write(block_num, data, &mut flush)
        }
        #[cfg(kernel_profile_extreme)]
        {
            let offset = block_num as u64 * state.block_size as u64;
            self.dev.write_bytes(offset, data).map_err(|_| FsError::IoError)
        }
    }

    /// Drop any cached copy of `block_num` **without** flushing it — the
    /// invalidate-on-free rule (design doc D-3): a freed block's stale bytes
    /// must never be written back over a block the allocator may hand out again.
    #[cfg(not(kernel_profile_extreme))]
    fn invalidate_block(&self, block_num: u32) {
        self.block_cache.lock().remove(block_num);
    }

    fn write_superblock(&self, state: &Ext2State) -> Result<(), FsError> {
        let mut buf = [0u8; size_of::<Superblock>()];
        state.superblock.serialize(&mut buf);
        self.dev.write_bytes(SUPERBLOCK_OFFSET, &buf).map_err(|_| FsError::IoError)?;
        Ok(())
    }

    fn bgd_offset(state: &Ext2State, group: u32) -> u64 {
        let bgd_table_block = state.first_data_block + 1;
        bgd_table_block as u64 * state.block_size as u64
            + group as u64 * size_of::<BlockGroupDescriptor>() as u64
    }

    /// Read a sub-block byte range. With a block cache this goes through it
    /// (the cached copy is authoritative under write-back); `extreme` reads
    /// the device. Caller guarantees the range lies within a single block —
    /// true for inode-table and BGD-table entries (entry size divides the
    /// block size and the tables start block-aligned, so neither ever
    /// straddles a block boundary).
    fn read_range(&self, state: &Ext2State, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        #[cfg(not(kernel_profile_extreme))]
        {
            let bs = state.block_size as u64;
            let block_num = (offset / bs) as u32;
            let off = (offset % bs) as usize;
            let len = buf.len();
            self.with_block(state, block_num, |block| {
                buf.copy_from_slice(&block[off..off + len]);
            })
        }
        #[cfg(kernel_profile_extreme)]
        {
            let _ = state; // no cache to consult on this profile
            self.dev.read_bytes(offset, buf).map_err(|_| FsError::IoError)
        }
    }

    /// Write a sub-block byte range. Cache builds: patch the cached block in
    /// place when resident (no read-back, no allocation — design doc D-5);
    /// otherwise fill-then-patch through the cache. `extreme` writes the
    /// device directly. Same single-block invariant as [`Self::read_range`].
    fn write_range(&self, state: &Ext2State, offset: u64, data: &[u8]) -> Result<(), FsError> {
        #[cfg(not(kernel_profile_extreme))]
        {
            let bs = state.block_size as u64;
            let block_num = (offset / bs) as u32;
            let off = (offset % bs) as usize;
            if self.block_cache.lock().patch(block_num, off, data) {
                return Ok(());
            }
            let mut block = self.read_block(state, block_num)?;
            block[off..off + data.len()].copy_from_slice(data);
            self.write_block(state, block_num, &block)?;
            Ok(())
        }
        #[cfg(kernel_profile_extreme)]
        {
            let _ = state; // no cache to patch on this profile
            self.dev.write_bytes(offset, data).map_err(|_| FsError::IoError)
        }
    }

    fn read_bgd(&self, state: &Ext2State, group: u32) -> Result<BlockGroupDescriptor, FsError> {
        let offset = Self::bgd_offset(state, group);
        let mut buf = [0u8; size_of::<BlockGroupDescriptor>()];
        self.read_range(state, offset, &mut buf)?;
        BlockGroupDescriptor::parse(&buf).ok_or(FsError::Corrupt)
    }

    fn write_bgd(&self, state: &Ext2State, group: u32, bgd: &BlockGroupDescriptor) -> Result<(), FsError> {
        let offset = Self::bgd_offset(state, group);
        let mut buf = [0u8; size_of::<BlockGroupDescriptor>()];
        bgd.serialize(&mut buf);
        self.write_range(state, offset, &buf)
    }

    // ========================================================================
    // Deferred metadata writeback
    // ========================================================================
    //
    // `read_bgd_staged` / `stage_bgd` / `stage_sb` are used *only* by the four
    // allocator functions (`allocate_block`, `free_block`, `allocate_inode`,
    // `free_inode`), which all hold `&mut Ext2State`. They keep the
    // authoritative BGD + superblock free counts in memory and defer the disk
    // write to `flush_meta`, called at the end of every mutating `Filesystem`
    // method and on `sync()`. Read-only callers keep using `read_bgd` and only
    // ever read `bgd.inode_table`, which no allocation changes — so a BGD that
    // is dirty-in-memory but stale-on-disk is invisible to them.

    /// BGD for `group`, from the in-memory cache if present, else read from disk
    /// and cached.
    fn read_bgd_staged(&self, state: &mut Ext2State, group: u32) -> Result<BlockGroupDescriptor, FsError> {
        let g = group as usize;
        if g >= state.bgd_cache.len() {
            return Err(FsError::Internal);
        }
        if let Some(bgd) = state.bgd_cache[g] {
            return Ok(bgd);
        }
        let bgd = self.read_bgd(state, group)?;
        state.bgd_cache[g] = Some(bgd);
        Ok(bgd)
    }

    /// Record an updated BGD in memory and mark it for the next `flush_meta`.
    fn stage_bgd(&self, state: &mut Ext2State, group: u32, bgd: &BlockGroupDescriptor) {
        let g = group as usize;
        if g < state.bgd_cache.len() {
            state.bgd_cache[g] = Some(*bgd);
            state.bgd_dirty[g] = true;
        }
    }

    /// Mark the (already updated in memory) superblock for the next `flush_meta`.
    fn stage_sb(state: &mut Ext2State) {
        state.sb_dirty = true;
    }

    /// Index of bitmap block `block_num` in `state.bitmap_cache`, loading it from
    /// disk on first touch. The allocators mutate `bitmap_cache[idx].1` in place
    /// and set `.2 = true`; `flush_meta` writes it back.
    fn bitmap_slot(&self, state: &mut Ext2State, block_num: u32) -> Result<usize, FsError> {
        if let Some(i) = state.bitmap_cache.iter().position(|(b, _, _)| *b == block_num) {
            return Ok(i);
        }
        let data = self.read_block(state, block_num)?;
        state.bitmap_cache.push((block_num, data, false));
        Ok(state.bitmap_cache.len() - 1)
    }

    /// Is `bn` an *allocation-metadata* block (a bitmap block, a BGD-table
    /// block, or the superblock's block)? Used by `flush_meta` to order
    /// write-back: file data + inode-table blocks reach the disk first, so a
    /// crash never leaves a bitmap/BGD claiming space whose data never landed
    /// (journal-less ordering, design doc D-2). The superblock's block is
    /// included defensively although nothing ever caches it (it is reserved,
    /// so no read/insert path names it).
    #[cfg(not(kernel_profile_extreme))]
    fn is_alloc_meta(state: &Ext2State, bn: u32) -> bool {
        if state.bitmap_cache.iter().any(|(b, _, _)| *b == bn) {
            return true;
        }
        let bgd_first = state.first_data_block + 1;
        let bgd_span = (state.block_group_count as usize * size_of::<BlockGroupDescriptor>()
            + state.block_size
            - 1)
            / state.block_size;
        if bn >= bgd_first && ((bn - bgd_first) as usize) < bgd_span {
            return true;
        }
        let sb_block = (SUPERBLOCK_OFFSET / state.block_size.max(1) as u64) as u32;
        bn == sb_block
    }

    /// Push every dirty cache block passing `keep` out to the device.
    #[cfg(not(kernel_profile_extreme))]
    fn flush_dirty_blocks(
        &self,
        state: &Ext2State,
        keep: &dyn Fn(u32) -> bool,
    ) -> Result<(), FsError> {
        let mut cache = self.block_cache.lock();
        let mut flush = self.dev_flush(state);
        cache.flush_dirty(keep, &mut flush)
    }

    /// Write every bitmap block, BGD, and the superblock that the allocators
    /// marked dirty, plus every dirty data/inode block in the cache. Cheap when
    /// nothing is dirty (a read, or a pure-rename op).
    ///
    /// Ordering (design doc D-2): dirty **data + inode-table + indirect**
    /// blocks are flushed to the device *first*, then bitmaps/BGDs/superblock
    /// are written and flushed. An unclean crash can then leak an allocated
    /// block (recoverable by e2fsck) but never publishes a bitmap claiming
    /// blocks whose contents never landed.
    fn flush_meta(&self, state: &mut Ext2State) -> Result<(), FsError> {
        // Phase 1 — everything dirty except allocation metadata.
        #[cfg(not(kernel_profile_extreme))]
        self.flush_dirty_blocks(state, &|bn| !Self::is_alloc_meta(state, bn))?;

        // Phase 2 — allocation metadata, staged by the allocators. Bitmap
        // blocks: move the cache out so `write_block(state, …)` can borrow
        // `state` while we iterate; put it back (with dirty flags cleared).
        if state.bitmap_cache.iter().any(|(_, _, d)| *d) {
            let mut bmaps = core::mem::take(&mut state.bitmap_cache);
            let mut err = Ok(());
            for (bn, data, dirty) in &mut bmaps {
                if *dirty {
                    if let e @ Err(_) = self.write_block(state, *bn, data) {
                        err = e;
                        break;
                    }
                    *dirty = false;
                }
            }
            state.bitmap_cache = bmaps;
            err?;
        }
        for group in 0..state.block_group_count {
            if !state.bgd_dirty[group as usize] {
                continue;
            }
            let bgd = state.bgd_cache[group as usize]
                .expect("bgd_dirty implies bgd_cache is populated");
            self.write_bgd(state, group, &bgd)?;
            state.bgd_dirty[group as usize] = false;
        }
        if state.sb_dirty {
            self.write_superblock(state)?;
            state.sb_dirty = false;
        }

        // Phase 3 — the metadata writes above went through the cache; push
        // them (and anything else left dirty) to the device.
        #[cfg(not(kernel_profile_extreme))]
        self.flush_dirty_blocks(state, &|_| true)?;

        Ok(())
    }

    /// `pub(crate)` for `mod tests`, which reads a record back the way `e2fsck`
    /// does — see [`Ext2Filesystem::remove_dir`]. Not reachable outside the crate.
    pub(crate) fn read_inode(&self, state: &Ext2State, inode_num: u32) -> Result<Inode, FsError> {
        if inode_num == 0 {
            return Err(FsError::NotFound);
        }
        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let index_in_group = inode_idx % state.inodes_per_group;

        let bgd = self.read_bgd(state, group)?;
        let inode_table = bgd.inode_table;

        let inode_offset = inode_table as u64 * state.block_size as u64
            + index_in_group as u64 * state.inode_size as u64;

        let mut buf = vec![0u8; state.inode_size as usize];
        self.read_range(state, inode_offset, &mut buf)?;

        Inode::parse(&buf).ok_or(FsError::Corrupt)
    }

    fn write_inode(&self, state: &Ext2State, inode_num: u32, inode: &Inode) -> Result<(), FsError> {
        if inode_num == 0 {
            return Err(FsError::NotFound);
        }
        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let index_in_group = inode_idx % state.inodes_per_group;

        let bgd = self.read_bgd(state, group)?;
        let inode_table = bgd.inode_table;

        let inode_offset = inode_table as u64 * state.block_size as u64
            + index_in_group as u64 * state.inode_size as u64;

        let mut buf = [0u8; size_of::<Inode>()];
        inode.serialize(&mut buf);
        self.write_range(state, inode_offset, &buf)
    }

    // ========================================================================
    // Bitmap Operations
    // ========================================================================

    fn get_bit(bitmap: &[u8], bit: u32) -> bool {
        let byte = bit / 8;
        let bit_offset = bit % 8;
        if (byte as usize) < bitmap.len() {
            (bitmap[byte as usize] & (1 << bit_offset)) != 0
        } else {
            true // Out of range = allocated
        }
    }

    fn set_bit(bitmap: &mut [u8], bit: u32, value: bool) {
        let byte = bit / 8;
        let bit_offset = bit % 8;
        if (byte as usize) < bitmap.len() {
            if value {
                bitmap[byte as usize] |= 1 << bit_offset;
            } else {
                bitmap[byte as usize] &= !(1 << bit_offset);
            }
        }
    }

    // ========================================================================
    // Block Allocation
    // ========================================================================

    /// Allocate one block and zero it on disk. Callers that immediately overwrite
    /// the whole block use [`Self::allocate_block_inner`] with `zero_new = false`.
    fn allocate_block(&self, state: &mut Ext2State) -> Result<u32, FsError> {
        self.allocate_block_inner(state, true)
    }

    /// `zero_new = false` skips the post-allocation zero-fill write — one full
    /// block write per allocation that is pure waste when the caller is about to
    /// write the entire block anyway (`write_inode_data`, the full-block arm of
    /// `write_at`). Never pass `false` for a metadata block (indirect pointer
    /// block, new directory block) or a partially-written data block.
    fn allocate_block_inner(&self, state: &mut Ext2State, zero_new: bool) -> Result<u32, FsError> {
        let unalloc = state.superblock.unallocated_blocks;
        if unalloc == 0 {
            return Err(FsError::NoSpace);
        }

        for group in 0..state.block_group_count {
            let mut bgd = self.read_bgd_staged(state, group)?;
            let free_count = bgd.free_blocks_count;
            if free_count == 0 {
                continue;
            }

            let bi = self.bitmap_slot(state, bgd.block_bitmap)?;

            // Find first free bit, resuming from the group's cursor and
            // wrapping once (D-6). The group has `free_count > 0`, so a free
            // bit exists and one of the two ranges finds it.
            let start = state.block_hint[group as usize];
            let scan = (start..state.blocks_per_group).chain(0..start);
            for bit in scan {
                if !Self::get_bit(&state.bitmap_cache[bi].1, bit) {
                    // Found free block — set the bit in memory, defer the write.
                    Self::set_bit(&mut state.bitmap_cache[bi].1, bit, true);
                    state.bitmap_cache[bi].2 = true;
                    state.block_hint[group as usize] = bit + 1;

                    // Update BGD + superblock in memory; flushed by flush_meta.
                    bgd.free_blocks_count = free_count - 1;
                    self.stage_bgd(state, group, &bgd);
                    state.superblock.unallocated_blocks = unalloc - 1;
                    Self::stage_sb(state);

                    let block_num = state.first_data_block + group * state.blocks_per_group + bit;

                    if zero_new {
                        let zeros = vec![0u8; state.block_size];
                        self.write_block(state, block_num, &zeros)?;
                    }

                    return Ok(block_num);
                }
            }
        }

        Err(FsError::NoSpace)
    }

    fn free_block(&self, state: &mut Ext2State, block_num: u32) -> Result<(), FsError> {
        if block_num == 0 {
            return Ok(());
        }

        // Block numbering starts at first_data_block (1 for 1024-byte blocks)
        let adjusted = block_num - state.first_data_block;
        let group = adjusted / state.blocks_per_group;
        let bit = adjusted % state.blocks_per_group;

        let mut bgd = self.read_bgd_staged(state, group)?;
        let bi = self.bitmap_slot(state, bgd.block_bitmap)?;
        Self::set_bit(&mut state.bitmap_cache[bi].1, bit, false);
        state.bitmap_cache[bi].2 = true;

        // Pull the scan cursor back so the freed block is the next candidate
        // (immediate reuse of deleted-file space).
        let hint = &mut state.block_hint[group as usize];
        if bit < *hint {
            *hint = bit;
        }

        // Invalidate-on-free (design doc D-3): drop any cached copy of the
        // freed block *without* flushing it. Flushing would push stale bytes
        // onto a block the allocator may hand out again before the flush
        // lands; dropping them is always safe (disk keeps the old contents
        // until reallocation, which always rewrites).
        #[cfg(not(kernel_profile_extreme))]
        self.invalidate_block(block_num);

        bgd.free_blocks_count += 1;
        self.stage_bgd(state, group, &bgd);
        state.superblock.unallocated_blocks += 1;
        Self::stage_sb(state);

        Ok(())
    }

    // ========================================================================
    // Inode Allocation
    // ========================================================================

    fn allocate_inode(&self, state: &mut Ext2State, is_dir: bool) -> Result<u32, FsError> {
        // Reclaim anything unlinked-while-mapped whose mappings have since gone.
        // Without this the filesystem could report itself full while holding
        // inodes nothing references any more, and — worse for a build workload —
        // an inode freed here is one the next `create` can reuse immediately.
        self.drain_deferred_frees(state);

        let unalloc = state.superblock.unallocated_inodes;
        if unalloc == 0 {
            return Err(FsError::NoSpace);
        }

        for group in 0..state.block_group_count {
            let mut bgd = self.read_bgd_staged(state, group)?;
            let free_count = bgd.free_inodes_count;
            if free_count == 0 {
                continue;
            }

            let bi = self.bitmap_slot(state, bgd.inode_bitmap)?;

            let start = state.inode_hint[group as usize];
            let scan = (start..state.inodes_per_group).chain(0..start);
            for bit in scan {
                if !Self::get_bit(&state.bitmap_cache[bi].1, bit) {
                    Self::set_bit(&mut state.bitmap_cache[bi].1, bit, true);
                    state.bitmap_cache[bi].2 = true;
                    state.inode_hint[group as usize] = bit + 1;

                    bgd.free_inodes_count = free_count - 1;
                    if is_dir {
                        bgd.used_dirs_count += 1;
                    }
                    self.stage_bgd(state, group, &bgd);
                    state.superblock.unallocated_inodes = unalloc - 1;
                    Self::stage_sb(state);

                    let inode_num = group * state.inodes_per_group + bit + 1;
                    return Ok(inode_num);
                }
            }
        }

        Err(FsError::NoSpace)
    }

    /// Inodes *this* filesystem has queued for a deferred free. The global
    /// [`deferred_free_pending`] counter aggregates every mount, so it cannot be
    /// asserted on by tests that run in parallel — this can.
    /// `(path walks, block-cache accesses)` this instance has done so far.
    ///
    /// The deterministic half of the read-path measurement: wall-clock on this
    /// host swings several-fold between runs (`README.md` § Performance), but
    /// "how many directory walks did these reads cost" does not move at all.
    #[cfg(test)]
    pub fn work_counters(&self) -> (u64, u64) {
        (
            self.counters.path_walks.load(Ordering::Relaxed),
            self.counters.block_accesses.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub fn deferred_free_len(&self) -> usize {
        self.deferred.slots.iter().filter(|s| s.load(Ordering::Relaxed) != 0).count()
    }

    /// Complete the frees deferred by [`Self::release_last_link`] for inodes
    /// whose last pin has since dropped.
    ///
    /// Called with the state write lock already held, from the two places that
    /// care: every unlink (which keeps the list short on a build workload) and
    /// every inode allocation (so a deferred free is reclaimed before the
    /// filesystem reports itself full).
    ///
    /// `is_pinned` is deliberately re-asked here rather than trusted from unlink
    /// time: a mapping created *after* the unlink still names this inode, and
    /// freeing it would reintroduce exactly the defect the deferral exists to
    /// prevent.
    fn drain_deferred_frees(&self, state: &mut Ext2State) {
        for slot in &self.deferred.slots {
            let inode_num = slot.load(Ordering::Acquire);
            if inode_num == 0 || akuma_primitives::inode_pin::is_pinned(inode_num) {
                continue;
            }
            // Claim it before doing any work, so a concurrent drain cannot free
            // the same inode twice.
            if slot
                .compare_exchange(inode_num, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            DEFERRED_FREE_PENDING.fetch_sub(1, Ordering::Relaxed);
            // Best-effort from here: a failure leaves the inode allocated but
            // unreferenced, which `e2fsck` reconnects. Re-queueing it on error
            // could spin on a permanently failing inode.
            if let Ok(mut inode) = self.read_inode(state, inode_num) {
                let _ = self.truncate_inode(state, &mut inode);
                inode.deletion_time = self.current_time();
                let _ = self.write_inode(state, inode_num, &inode);
            }
            let _ = self.free_inode(state, inode_num, false);
        }
    }

    /// Drop the last directory-entry reference to `inode_num`, whose link count
    /// the caller has already decremented to zero, and write the inode back.
    ///
    /// A live reader may still name this inode. Linux keeps an unlinked inode
    /// alive until its last reference goes; this kernel has no open-file object
    /// to hang that on, so an [`akuma_primitives::inode_pin::InodePin`] is the
    /// reference — held by every lazy mmap region and, since per-fd inode
    /// caching, by every open `File` descriptor.
    ///
    /// Freeing regardless is what produced root cause #2 of the self-host ICE:
    /// the truncate zeroes `i_size`, so the reader's next fill gets `Ok(0)` and
    /// installs a zero page, and once `free_inode` returns the number to the
    /// bitmap the next file created inherits it, handing that reader another
    /// file's bytes (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14). So for a
    /// pinned inode: drop the name, keep the inode. Size and block pointers stay
    /// exactly as they are, which is what lets the reader go on reading correct
    /// data, and [`Self::drain_deferred_frees`] finishes the job once the last
    /// pin drops.
    ///
    /// **This is one function because it has two callers.** `remove_file` had
    /// the pin check; `rename` — which unlinks its destination's last name in
    /// exactly the same way — did not, and freed it outright. That is the
    /// atomic-replace pattern (`write foo.tmp`, `rename foo.tmp foo`) that
    /// `cargo`, `apk` and every editor use, so the one path most likely to pull
    /// an inode out from under a live reader was the path that never checked.
    /// The fast-symlink guard was asymmetric the same way: `truncate_inode`
    /// reads `direct_blocks` as block numbers, but a fast symlink stores its
    /// target *string* there, so truncating one frees whatever blocks those
    /// characters happen to spell — `remove_file` guarded, `rename` did not.
    ///
    /// `flush_meta` stays with the caller, which has more to stage than this.
    fn release_last_link(
        &self,
        state: &mut Ext2State,
        inode_num: u32,
        inode: &mut Inode,
    ) -> Result<(), FsError> {
        if akuma_primitives::inode_pin::is_pinned(inode_num) {
            // If the deferral list is full the inode is *leaked*, not freed: it
            // stays allocated with no name and no queued free, and only `e2fsck`
            // reclaims it. `DeferredFrees::push` counts that case. Leaking blocks
            // is recoverable; handing a reader another file's bytes is not, so
            // the overflow falls this way deliberately.
            self.deferred.push(inode_num);
            inode.hard_links = 0;
            return self.write_inode(state, inode_num, inode);
        }

        let is_fast_symlink = (inode.type_perms & 0xF000) == S_IFLNK
            && inode.sectors_used == 0
            && (inode.size_lower as usize) <= FAST_SYMLINK_MAX;
        if !is_fast_symlink {
            self.truncate_inode(state, inode)?;
        }
        inode.deletion_time = self.current_time();
        self.write_inode(state, inode_num, inode)?;
        self.free_inode(state, inode_num, false)
    }

    fn free_inode(&self, state: &mut Ext2State, inode_num: u32, is_dir: bool) -> Result<(), FsError> {
        if inode_num == 0 {
            return Ok(());
        }

        // The single choke point where a number returns to the allocator, and so
        // the only correct place to drop anything keyed on it — see
        // [`InodeFreedHook`]. Safe from a publish-after-invalidate race precisely
        // because of the pin: a fill in flight holds a cloned `LazySource`, which
        // holds a pin, so an inode reaching here has no fill that could republish.
        on_inode_freed(inode_num);

        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let bit = inode_idx % state.inodes_per_group;

        let mut bgd = self.read_bgd_staged(state, group)?;
        let bi = self.bitmap_slot(state, bgd.inode_bitmap)?;
        Self::set_bit(&mut state.bitmap_cache[bi].1, bit, false);
        state.bitmap_cache[bi].2 = true;

        let hint = &mut state.inode_hint[group as usize];
        if bit < *hint {
            *hint = bit;
        }

        bgd.free_inodes_count += 1;
        if is_dir && bgd.used_dirs_count > 0 {
            bgd.used_dirs_count -= 1;
        }
        self.stage_bgd(state, group, &bgd);
        state.superblock.unallocated_inodes += 1;
        Self::stage_sb(state);

        Ok(())
    }

    // ========================================================================
    // Block Mapping (logical -> physical)
    // ========================================================================

    fn get_block_num(&self, 
        state: &Ext2State,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<u32>, FsError> {
        let ptrs_per_block = (state.block_size / 4) as u32;

        if logical_block < 12 {
            let block = inode.direct_blocks[logical_block as usize];
            return Ok(if block == 0 { None } else { Some(block) });
        }

        let logical_block = logical_block - 12;

        if logical_block < ptrs_per_block {
            if inode.indirect_block == 0 {
                return Ok(None);
            }
            let idx = logical_block as usize;
            let block =
                self.with_block(state, inode.indirect_block, |b| Self::read_block_ptr(b, idx))?;
            return Ok(if block == 0 { None } else { Some(block) });
        }

        let logical_block = logical_block - ptrs_per_block;

        if logical_block < ptrs_per_block * ptrs_per_block {
            if inode.double_indirect_block == 0 {
                return Ok(None);
            }
            let idx1 = (logical_block / ptrs_per_block) as usize;
            let idx2 = (logical_block % ptrs_per_block) as usize;

            let indirect_block = self
                .with_block(state, inode.double_indirect_block, |b| Self::read_block_ptr(b, idx1))?;
            if indirect_block == 0 {
                return Ok(None);
            }

            let block = self.with_block(state, indirect_block, |b| Self::read_block_ptr(b, idx2))?;
            return Ok(if block == 0 { None } else { Some(block) });
        }

        Err(FsError::NotSupported)
    }

    fn read_block_ptr(block: &[u8], index: usize) -> u32 {
        let offset = index * 4;
        u32::from_le_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ])
    }

    fn write_block_ptr(block: &mut [u8], index: usize, value: u32) {
        let offset = index * 4;
        let bytes = value.to_le_bytes();
        block[offset..offset + 4].copy_from_slice(&bytes);
    }

    /// Ensure a block exists at the given logical position, allocating if needed.
    ///
    /// `zero_leaf` controls only the **data** block: `false` skips its
    /// post-allocation zero-fill for callers that write the whole block
    /// immediately afterward (see [`Self::allocate_block_inner`]). Indirect and
    /// double-indirect pointer blocks are always zeroed regardless.
    fn ensure_block(&self,
        state: &mut Ext2State,
        inode: &mut Inode,
        logical_block: u32,
        zero_leaf: bool,
    ) -> Result<u32, FsError> {
        let ptrs_per_block = (state.block_size / 4) as u32;

        if logical_block < 12 {
            if inode.direct_blocks[logical_block as usize] == 0 {
                let new_block = self.allocate_block_inner(state, zero_leaf)?;
                inode.direct_blocks[logical_block as usize] = new_block;
                inode.sectors_used += (state.block_size / 512) as u32;
            }
            return Ok(inode.direct_blocks[logical_block as usize]);
        }

        let lb = logical_block - 12;

        if lb < ptrs_per_block {
            // Singly indirect
            if inode.indirect_block == 0 {
                inode.indirect_block = self.allocate_block(state)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            let mut indirect = self.read_block(state, inode.indirect_block)?;
            let mut block = Self::read_block_ptr(&indirect, lb as usize);

            if block == 0 {
                block = self.allocate_block_inner(state, zero_leaf)?;
                Self::write_block_ptr(&mut indirect, lb as usize, block);
                self.write_block(state, inode.indirect_block, &indirect)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            return Ok(block);
        }

        let lb = lb - ptrs_per_block;

        if lb < ptrs_per_block * ptrs_per_block {
            // Doubly indirect
            if inode.double_indirect_block == 0 {
                inode.double_indirect_block = self.allocate_block(state)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            let idx1 = (lb / ptrs_per_block) as usize;
            let idx2 = (lb % ptrs_per_block) as usize;

            let mut double_indirect = self.read_block(state, inode.double_indirect_block)?;
            let mut indirect_block = Self::read_block_ptr(&double_indirect, idx1);

            if indirect_block == 0 {
                indirect_block = self.allocate_block(state)?;
                Self::write_block_ptr(&mut double_indirect, idx1, indirect_block);
                self.write_block(state, inode.double_indirect_block, &double_indirect)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            let mut indirect = self.read_block(state, indirect_block)?;
            let mut block = Self::read_block_ptr(&indirect, idx2);

            if block == 0 {
                block = self.allocate_block_inner(state, zero_leaf)?;
                Self::write_block_ptr(&mut indirect, idx2, block);
                self.write_block(state, indirect_block, &indirect)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            return Ok(block);
        }

        Err(FsError::NotSupported)
    }

    // ========================================================================
    // Inode Data Operations
    // ========================================================================

    fn read_inode_data(&self, state: &Ext2State, inode: &Inode) -> Result<Vec<u8>, FsError> {
        let size = inode.size_lower as usize;

        // Safety: Limit maximum kernel-side file allocation to 16MB to prevent OOM/panic.
        // Large files should be accessed via read_at() instead of read_file().
        if size > 16 * 1024 * 1024 {
            return Err(FsError::Internal);
        }

        let mut data = Vec::with_capacity(size);
        let blocks_needed = (size + state.block_size - 1) / state.block_size;

        for logical_block in 0..blocks_needed as u32 {
            if let Some(phys_block) = self.get_block_num(state, inode, logical_block)? {
                let remaining = size - data.len();
                let to_copy = core::cmp::min(remaining, state.block_size);
                self.with_block(state, phys_block, |b| data.extend_from_slice(&b[..to_copy]))?;
            } else {
                let remaining = size - data.len();
                let to_copy = core::cmp::min(remaining, state.block_size);
                data.extend(core::iter::repeat(0).take(to_copy));
            }
        }

        Ok(data)
    }

    fn write_inode_data(&self,
        state: &mut Ext2State,
        inode_num: u32,
        inode: &mut Inode,
        data: &[u8],
    ) -> Result<(), FsError> {
        let blocks_needed = (data.len() + state.block_size - 1) / state.block_size;

        for logical_block in 0..blocks_needed as u32 {
            // `zero_leaf = false`: every iteration writes a full block below
            // (zero-padded past `data`), so the allocator's zero-fill is redundant.
            let phys_block = match self.ensure_block(state, inode, logical_block, false) {
                Ok(b) => b,
                Err(e) => {
                    return Err(e);
                }
            };

            let start = logical_block as usize * state.block_size;
            let end = core::cmp::min(start + state.block_size, data.len());

            let mut block_data = vec![0u8; state.block_size];
            block_data[..end - start].copy_from_slice(&data[start..end]);

            if let Err(e) = self.write_block(state, phys_block, &block_data) {
                return Err(e);
            }
        }

        inode.size_lower = data.len() as u32;
        let now = self.current_time();
        inode.modification_time = now;
        self.write_inode(state, inode_num, inode)?;

        Ok(())
    }

    fn truncate_inode(&self, state: &mut Ext2State, inode: &mut Inode) -> Result<(), FsError> {
        // Free all direct blocks
        // Indexed, not iterated, and clippy's `needless_range_loop` cannot be
        // honoured here: `Inode` is packed, so `&mut inode.direct_blocks[..]`
        // is a reference to a field of a packed struct — rejected as unaligned
        // (E0793), and UB even if never dereferenced. Copy-in/copy-out through
        // the index is the only sound form.
        #[allow(clippy::needless_range_loop)]
        for i in 0..12 {
            if inode.direct_blocks[i] != 0 {
                self.free_block(state, inode.direct_blocks[i])?;
                inode.direct_blocks[i] = 0;
            }
        }

        // Free indirect block and its contents
        if inode.indirect_block != 0 {
            let ptrs_per_block = state.block_size / 4;
            let indirect = self.read_block(state, inode.indirect_block)?;
            for i in 0..ptrs_per_block {
                let block = Self::read_block_ptr(&indirect, i);
                if block != 0 {
                    self.free_block(state, block)?;
                }
            }
            self.free_block(state, inode.indirect_block)?;
            inode.indirect_block = 0;
        }

        // Free double indirect (simplified - just free the pointer block)
        if inode.double_indirect_block != 0 {
            let ptrs_per_block = state.block_size / 4;
            let double_indirect = self.read_block(state, inode.double_indirect_block)?;
            for i in 0..ptrs_per_block {
                let indirect_block = Self::read_block_ptr(&double_indirect, i);
                if indirect_block != 0 {
                    let indirect = self.read_block(state, indirect_block)?;
                    for j in 0..ptrs_per_block {
                        let block = Self::read_block_ptr(&indirect, j);
                        if block != 0 {
                            self.free_block(state, block)?;
                        }
                    }
                    self.free_block(state, indirect_block)?;
                }
            }
            self.free_block(state, inode.double_indirect_block)?;
            inode.double_indirect_block = 0;
        }

        inode.size_lower = 0;
        inode.sectors_used = 0;

        Ok(())
    }

    // ========================================================================
    // Directory Operations
    // ========================================================================

    fn parse_directory(&self, data: &[u8]) -> Vec<(u32, String, u8)> {
        let mut entries = Vec::new();
        let mut offset = 0;

        while offset + DIR_ENTRY_HEADER_SIZE <= data.len() {
            let entry: DirEntryRaw = DirEntryRaw::parse(&data[offset..])
                .expect("loop guard promises DIR_ENTRY_HEADER_SIZE bytes");

            if entry.rec_len == 0 {
                break;
            }

            if entry.inode != 0 {
                let name_start = offset + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + entry.name_len as usize;

                if name_end <= data.len() {
                    if let Ok(name) = core::str::from_utf8(&data[name_start..name_end]) {
                        entries.push((entry.inode, name.to_string(), entry.file_type));
                    }
                }
            }

            offset += entry.rec_len as usize;
        }

        entries
    }

    /// Write back only the directory blocks overlapping bytes `[start, end)` of
    /// `dir_data`, instead of every block (`write_inode_data`). A dirent edit
    /// touches one block; rewriting the whole directory made `add_dir_entry` /
    /// `remove_dir_entry` O(directory size) per call — O(N²) to fill or empty a
    /// directory. `dir_data` must otherwise match what is on disk (it came from
    /// `read_inode_data`). Grows the directory (allocating the block) when
    /// `dir_data` is longer than `i_size` — the append case.
    fn write_dir_range(
        &self,
        state: &mut Ext2State,
        dir_inode_num: u32,
        dir_inode: &mut Inode,
        dir_data: &[u8],
        start: usize,
        end: usize,
    ) -> Result<(), FsError> {
        let bs = state.block_size;
        debug_assert!(start < end && end <= dir_data.len());
        for lb in (start / bs)..=((end - 1) / bs) {
            // zero_leaf = false: the whole block is written just below.
            let phys = self.ensure_block(state, dir_inode, lb as u32, false)?;
            let s = lb * bs;
            let e = core::cmp::min(s + bs, dir_data.len());
            let mut block = vec![0u8; bs];
            block[..e - s].copy_from_slice(&dir_data[s..e]);
            self.write_block(state, phys, &block)?;
        }
        if dir_data.len() as u32 > dir_inode.size_lower {
            dir_inode.size_lower = dir_data.len() as u32;
        }
        dir_inode.modification_time = self.current_time();
        self.write_inode(state, dir_inode_num, dir_inode)
    }

    fn add_dir_entry(&self,
        state: &mut Ext2State,
        dir_inode_num: u32,
        name: &str,
        inode_num: u32,
        file_type: u8,
    ) -> Result<(), FsError> {
        let mut dir_inode = self.read_inode(state, dir_inode_num)?;
        let mut dir_data = self.read_inode_data(state, &dir_inode)?;

        let name_bytes = name.as_bytes();
        let needed_len = DIR_ENTRY_HEADER_SIZE + name_bytes.len();
        let aligned_len = (needed_len + 3) & !3; // Align to 4 bytes

        // Try to find space in existing entries
        let mut offset = 0;
        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let entry: DirEntryRaw = DirEntryRaw::parse(&dir_data[offset..])
                .expect("loop guard promises DIR_ENTRY_HEADER_SIZE bytes");

            if entry.rec_len == 0 {
                break;
            }

            let actual_len = if entry.inode == 0 {
                0
            } else {
                (DIR_ENTRY_HEADER_SIZE + entry.name_len as usize + 3) & !3
            };

            let free_space = entry.rec_len as usize - actual_len;

            if free_space >= aligned_len {
                // Split this entry. Everything changed lies inside this one
                // directory block (an entry never crosses a block boundary).
                let edit_start = offset;
                if entry.inode != 0 {
                    // Shrink existing entry
                    let new_rec_len = actual_len as u16;
                    dir_data[offset + 4] = new_rec_len as u8;
                    dir_data[offset + 5] = (new_rec_len >> 8) as u8;

                    offset += actual_len;
                }

                // Write new entry
                let new_entry = DirEntryRaw {
                    inode: inode_num,
                    rec_len: (entry.rec_len as usize - actual_len) as u16,
                    name_len: name_bytes.len() as u8,
                    file_type,
                };

                new_entry.serialize(&mut dir_data[offset..offset + DIR_ENTRY_HEADER_SIZE]);
                dir_data[offset + DIR_ENTRY_HEADER_SIZE
                    ..offset + DIR_ENTRY_HEADER_SIZE + name_bytes.len()]
                    .copy_from_slice(name_bytes);

                let edit_end = offset + DIR_ENTRY_HEADER_SIZE + name_bytes.len();
                self.write_dir_range(
                    state, dir_inode_num, &mut dir_inode, &dir_data, edit_start, edit_end,
                )?;
                return Ok(());
            }

            offset += entry.rec_len as usize;
        }

        // Need to allocate a new block for the directory
        let new_size = dir_data.len() + state.block_size;
        dir_data.resize(new_size, 0);

        // Write new entry at the start of the new block
        let new_block_offset = new_size - state.block_size;
        let new_entry = DirEntryRaw {
            inode: inode_num,
            rec_len: state.block_size as u16,
            name_len: name_bytes.len() as u8,
            file_type,
        };

        new_entry.serialize(&mut dir_data[new_block_offset..new_block_offset + DIR_ENTRY_HEADER_SIZE]);
        dir_data[new_block_offset + DIR_ENTRY_HEADER_SIZE
            ..new_block_offset + DIR_ENTRY_HEADER_SIZE + name_bytes.len()]
            .copy_from_slice(name_bytes);

        // Only the newly appended block changed.
        self.write_dir_range(
            state, dir_inode_num, &mut dir_inode, &dir_data, new_block_offset, new_size,
        )?;
        Ok(())
    }

    fn remove_dir_entry(&self,
        state: &mut Ext2State,
        dir_inode_num: u32,
        name: &str,
    ) -> Result<u32, FsError> {
        let mut dir_inode = self.read_inode(state, dir_inode_num)?;
        let mut dir_data = self.read_inode_data(state, &dir_inode)?;

        let mut offset = 0;
        let mut prev_offset: Option<usize> = None;

        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let entry: DirEntryRaw = DirEntryRaw::parse(&dir_data[offset..])
                .expect("loop guard promises DIR_ENTRY_HEADER_SIZE bytes");

            if entry.rec_len == 0 {
                break;
            }

            if entry.inode != 0 {
                let name_start = offset + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + entry.name_len as usize;

                if name_end <= dir_data.len() {
                    if let Ok(entry_name) = core::str::from_utf8(&dir_data[name_start..name_end]) {
                        if entry_name == name {
                            let removed_inode = entry.inode;
                            let bs = state.block_size;

                            // Merge into the previous entry only when it is in
                            // the *same* directory block — a dirent's rec_len
                            // must never cross a block boundary. Otherwise (the
                            // removed entry is first in its block) just clear
                            // the inode field, leaving a gap `add_dir_entry`
                            // can reuse.
                            let (edit_start, edit_end) = match prev_offset {
                                Some(prev) if prev / bs == offset / bs => {
                                    let prev_entry: DirEntryRaw = DirEntryRaw::parse(&dir_data[prev..])
                                        .expect("prev points at a header the scan already read");
                                    let new_rec_len = prev_entry.rec_len + entry.rec_len;
                                    dir_data[prev + 4] = new_rec_len as u8;
                                    dir_data[prev + 5] = (new_rec_len >> 8) as u8;
                                    (prev, prev + DIR_ENTRY_HEADER_SIZE)
                                }
                                _ => {
                                    dir_data[offset] = 0;
                                    dir_data[offset + 1] = 0;
                                    dir_data[offset + 2] = 0;
                                    dir_data[offset + 3] = 0;
                                    (offset, offset + 4)
                                }
                            };

                            self.write_dir_range(
                                state, dir_inode_num, &mut dir_inode, &dir_data, edit_start, edit_end,
                            )?;
                            return Ok(removed_inode);
                        }
                    }
                }
            }

            if entry.inode != 0 {
                prev_offset = Some(offset);
            }
            offset += entry.rec_len as usize;
        }

        Err(FsError::NotFound)
    }

    // ========================================================================
    // Symlink Operations
    // ========================================================================

    fn create_symlink_internal(&self, state: &mut Ext2State, parent_inode: u32, name: &str, target: &str) -> Result<(), FsError> {
        let target_bytes = target.as_bytes();
        let inode_num = self.allocate_inode(state, false)?;

        let now = self.current_time();
        let mut inode = Inode {
            type_perms: DEFAULT_SYMLINK_PERMS,
            uid: 0,
            size_lower: target_bytes.len() as u32,
            access_time: now,
            creation_time: now,
            modification_time: now,
            hard_links: 1,
            sectors_used: 0,
            ..Default::default()
        };

        if target_bytes.len() <= FAST_SYMLINK_MAX {
            // Fast symlink: store the target across the 15 pointer words —
            // bytes 40..100 of the serialized inode, Linux's exact convention
            // (docs/archive/AKUMA_EXT2_CLEANUP.md §3).
            inode.set_fast_symlink_target(target_bytes);
        } else {
            // Slow symlink: allocate data block(s)
            self.write_inode_data(state, inode_num, &mut inode, target_bytes)?;
        }

        self.write_inode(state, inode_num, &inode)?;
        self.add_dir_entry(state, parent_inode, name, inode_num, FT_SYMLINK)?;
        Ok(())
    }

    // ========================================================================
    // Socket node operations
    // ========================================================================

    /// Create a zero-length `S_IFSOCK` node — the filesystem presence of an
    /// AF_UNIX `bind(2)` on a pathname.
    ///
    /// Deliberately NOT a variant of file creation with a different mode: the
    /// node must never be openable as a file. It holds no data and allocates no
    /// data blocks, and the only things that read it are `stat` (for
    /// `S_ISSOCK`), `unlink`, and directory listings. `connect` resolves against
    /// the kernel's AF_UNIX name table, not against this inode — the node exists
    /// so that userspace conventions work (a client checking `S_ISSOCK`, a
    /// daemon `unlink`ing a stale path, `ls -l` showing an `s`), not as the
    /// socket itself.
    fn create_socket_node_internal(
        &self,
        state: &mut Ext2State,
        parent_inode: u32,
        name: &str,
    ) -> Result<(), FsError> {
        let inode_num = self.allocate_inode(state, false)?;
        let now = self.current_time();
        let inode = Inode {
            type_perms: DEFAULT_SOCKET_PERMS,
            uid: 0,
            size_lower: 0,
            access_time: now,
            creation_time: now,
            modification_time: now,
            hard_links: 1,
            sectors_used: 0,
            ..Default::default()
        };
        self.write_inode(state, inode_num, &inode)?;
        self.add_dir_entry(state, parent_inode, name, inode_num, FT_SOCK)?;
        Ok(())
    }

    fn read_symlink_inode(&self, state: &Ext2State, inode_num: u32) -> Result<String, FsError> {
        let inode = self.read_inode(state, inode_num)?;
        if (inode.type_perms & 0xF000) != S_IFLNK {
            return Err(FsError::NotAFile);
        }
        let len = inode.size_lower as usize;
        if inode.sectors_used == 0 && len <= FAST_SYMLINK_MAX {
            // Fast symlink: the target is in the 15 pointer words — bytes
            // 40..100 of the serialized inode, not just `direct_blocks`.
            let buf = inode.fast_symlink_target(len);
            let s = core::str::from_utf8(&buf[..len]).map_err(|_| FsError::IoError)?;
            Ok(String::from(s))
        } else {
            // Slow symlink
            let data = self.read_inode_data(state, &inode)?;
            let s = core::str::from_utf8(&data[..len]).map_err(|_| FsError::IoError)?;
            Ok(String::from(s))
        }
    }

    // ========================================================================
    // Path Resolution
    // ========================================================================

    /// `pub(crate)` for `mod tests`; not reachable outside the crate.
    pub(crate) fn lookup_path(&self, path: &str) -> Result<u32, FsError> {
        // Block for read lock. try_lock + IoError under concurrent writers caused
        // spurious EIO on /tmp (forktest combined_stress): readers starved while
        // another thread held write_state. Orphaned write locks are recovered in
        // read_state / write_state loops.
        let state = self.read_state();
        self.lookup_path_internal(&state, path)
    }

    fn lookup_path_internal(&self, state: &Ext2State, path: &str) -> Result<u32, FsError> {
        #[cfg(test)]
        self.counters.path_walks.fetch_add(1, Ordering::Relaxed);
        // `path_components` is an iterator, so this walk allocates nothing —
        // it runs on every path resolution. The empty-path case needs no
        // special handling: zero components means the loop never runs and
        // `ROOT_INODE` falls through, which is what the old explicit
        // `is_empty()` early-return did.
        let mut current_inode = ROOT_INODE;

        for component in path_components(path) {
            let inode = self.read_inode(state, current_inode)?;

            if (inode.type_perms & 0xF000) != S_IFDIR {
                return Err(FsError::NotADirectory);
            }

            let dir_data = self.read_inode_data(state, &inode)?;
            let entries = self.parse_directory(&dir_data);

            let found = entries.iter().find(|(_, name, _)| name == component);

            match found {
                Some((inode_num, _, _)) => current_inode = *inode_num,
                None => return Err(FsError::NotFound),
            }
        }

        Ok(current_inode)
    }

    fn lookup_parent_internal(&self, state: &Ext2State, path: &str) -> Result<(u32, String), FsError> {
        let (parent_path, name) = split_path(path);
        if name.is_empty() {
            return Err(FsError::InvalidPath);
        }
        let parent_path = if parent_path.is_empty() {
            "/"
        } else {
            parent_path
        };
        let parent_inode = self.lookup_path_internal(state, parent_path)?;
        Ok((parent_inode, name.to_string()))
    }

    fn lookup_parent(&self, path: &str) -> Result<(u32, String), FsError> {
        let state = self.read_state();
        self.lookup_parent_internal(&state, path)
    }

    pub fn resolve_inode(&self, path: &str) -> Result<u32, FsError> {
        self.lookup_path(path)
    }

    /// The `Metadata` view of one inode. Shared by `metadata` (which looks the
    /// number up by path first) and `metadata_by_inode` (which is handed it), so
    /// an fd's `fstat` and a path's `stat` can never disagree about the same file.
    fn metadata_of(&self, state: &Ext2State, inode_num: u32) -> Result<Metadata, FsError> {
        let inode = self.read_inode(state, inode_num)?;
        Ok(Metadata {
            is_dir: (inode.type_perms & 0xF000) == S_IFDIR,
            size: inode.size_lower as u64,
            inode: inode_num as u64,
            mode: inode.type_perms as u32,
            created: Some(inode.creation_time as u64),
            modified: Some(inode.modification_time as u64),
            accessed: Some(inode.access_time as u64),
        })
    }

    pub fn read_at_by_inode(&self, inode_num: u32, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        if buf.is_empty() {
            return Ok(0);
        }

        let state = self.read_state();
        let inode = self.read_inode(&state, inode_num)?;

        // Same refusal `read_at` makes for a path naming a directory. Until
        // `read(2)` grew a per-fd inode (`KernelFile::inode`) the only callers
        // here were the mmap/exec fill paths, which never name a directory, so
        // the check was merely absent rather than wrong. Now a `read(2)` on a
        // directory fd reaches this function, and without it the caller would
        // get raw dirent bytes instead of `EISDIR`.
        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        let file_size = inode.size_lower as usize;
        if offset >= file_size {
            // Diagnostic (2026-08-15 hunt): a demand-fill clamps to `filesz`
            // resolved at mmap time, so `offset > file_size` here means this
            // inode's size is smaller NOW than the caller believed then — an
            // i_size incoherence (file truncated or inode freed+reused under a
            // live mapping), not a normal EOF. The `offset == file_size` case
            // is every ordinary read-at-end and stays silent and uncounted, or
            // this counter would be noise.
            if offset > file_size {
                let prev = E2_READ_AT_EOF.fetch_add(1, Ordering::Relaxed);
                if prev < 32 {
                    akuma_primitives::safe_print!(224,
                        "[E2-EOF] inode={} off={:#x} size_now={:#x} — caller believed the file extended past off\n",
                        inode_num, offset, file_size);
                }
            }
            return Ok(0);
        }

        let block_size = state.block_size;
        let end = core::cmp::min(offset + buf.len(), file_size);
        let first_logical = (offset / block_size) as u32;
        let last_logical = ((end - 1) / block_size) as u32;
        let num_blocks = (last_logical - first_logical + 1) as usize;

        // Resolve all logical->physical block mappings upfront
        let mut phys_blocks = Vec::with_capacity(num_blocks);
        for i in 0..num_blocks {
            phys_blocks.push(self.get_block_num(&state, &inode, first_logical + i as u32)?);
        }

        let mut total_read = 0usize;
        let mut pos = offset;
        let mut block_idx = 0usize;

        while block_idx < num_blocks {
            let offset_in_block = pos % block_size;

            if phys_blocks[block_idx].is_none() {
                let chunk = core::cmp::min(block_size - offset_in_block, end - pos);
                buf[total_read..total_read + chunk].fill(0);
                pos += chunk;
                total_read += chunk;
                block_idx += 1;
                continue;
            }

            let run_start_phys = phys_blocks[block_idx].unwrap();

            // Cache hit: copy this block straight from the cache (the warm self-host
            // path — the toolchain stays resident, so this is a memcpy, no disk I/O).
            #[cfg(not(kernel_profile_extreme))]
            {
                let cache = self.block_cache.lock();
                if let Some(data) = cache.get(run_start_phys) {
                    let chunk = core::cmp::min(block_size - offset_in_block, end - pos);
                    buf[total_read..total_read + chunk]
                        .copy_from_slice(&data[offset_in_block..offset_in_block + chunk]);
                    // The snapshot exists only for `[E2C-BAD]` verification (off
                    // by default — see E2_VERIFY_HITS); without it this path
                    // allocates nothing the original did not.
                    let hit_snapshot = if E2_VERIFY_HITS.load(Ordering::Relaxed) {
                        Some(data.to_vec())
                    } else {
                        None
                    };
                    drop(cache);
                    if let Some(snap) = &hit_snapshot {
                        self.verify_cached_block(&state, run_start_phys, snap);
                    }
                    #[cfg(any(ext2_fs_cache, test))]
                    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                    pos += chunk;
                    total_read += chunk;
                    block_idx += 1;
                    continue;
                }
            }

            // Cache miss: extend a run of physically-contiguous blocks that are ALSO
            // uncached, so one disk read batches only the bytes we actually need.
            // (With no cache compiled in, the cache check is gone and this is the
            // original whole-contiguous-run read.)
            let mut run_len = 1usize;
            while block_idx + run_len < num_blocks {
                match phys_blocks[block_idx + run_len] {
                    Some(next_phys) if next_phys == run_start_phys + run_len as u32 => {
                        #[cfg(not(kernel_profile_extreme))]
                        {
                            if self.block_cache.lock().get(next_phys).is_some() {
                                break;
                            }
                        }
                        run_len += 1;
                    }
                    _ => break,
                }
            }

            // Single disk read for the entire contiguous run
            let disk_offset = run_start_phys as u64 * block_size as u64;
            let run_bytes = run_len * block_size;
            let mut run_buf = alloc::vec![0u8; run_bytes];
            self.dev.read_bytes(disk_offset, &mut run_buf).map_err(|_| FsError::IoError)?;
            #[cfg(any(ext2_fs_cache, test))]
            CACHE_MISSES.fetch_add(run_len as u64, Ordering::Relaxed);

            // Copy relevant data from the run into output and populate the cache.
            let mut run_pos = 0usize;
            for _ in 0..run_len {
                let off = if run_pos == 0 { offset_in_block } else { 0 };
                let chunk = core::cmp::min(block_size - off, end - pos);
                buf[total_read..total_read + chunk]
                    .copy_from_slice(&run_buf[run_pos + off..run_pos + off + chunk]);
                #[cfg(not(kernel_profile_extreme))]
                {
                    let bn = run_start_phys + (run_pos / block_size) as u32;
                    let mut cache = self.block_cache.lock();
                    let mut flush = self.dev_flush(&state);
                    cache.insert(bn, &run_buf[run_pos..run_pos + block_size], &mut flush)?;
                }
                pos += chunk;
                total_read += chunk;
                run_pos += block_size;
            }

            block_idx += run_len;
        }

        Ok(total_read)
    }
}


// ============================================================================
// Filesystem Trait Implementation
// ============================================================================

impl<B: BlockDevice> Filesystem for Ext2Filesystem<B> {
    fn name(&self) -> &str {
        "ext2"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let state = self.read_state();
        let inode_num = self.lookup_path_internal(&state, path)?;
        let inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) != S_IFDIR {
            return Err(FsError::NotADirectory);
        }

        let dir_data = self.read_inode_data(&state, &inode)?;
        let raw_entries = self.parse_directory(&dir_data);

        let entries = raw_entries
            .into_iter()
            .filter(|(inode, name, _)| *inode != 0 && name != "." && name != "..")
            .map(|(inode_num, name, file_type)| {
                let is_dir = file_type == FT_DIR;
                let is_symlink = file_type == FT_SYMLINK;
                let size = if is_dir {
                    0
                } else {
                    self.read_inode(&state, inode_num)
                        .map_or(0, |i| i.size_lower as u64)
                };
                DirEntry { name, is_dir, is_symlink, size }
            })
            .collect();

        Ok(entries)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let state = self.read_state();
        let inode_num = self.lookup_path_internal(&state, path)?;
        let inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        self.read_inode_data(&state, &inode)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        // Try to find existing file
        match self.lookup_path(path) {
            Ok(inode_num) => {
                // File exists - truncate and write
                let mut state = self.write_state();
                let mut inode = self.read_inode(&state, inode_num)?;

                if (inode.type_perms & 0xF000) == S_IFDIR {
                    return Err(FsError::NotAFile);
                }

                self.truncate_inode(&mut state, &mut inode)?;
                self.write_inode_data(&mut state, inode_num, &mut inode, data)?;
                self.flush_meta(&mut state)?;
                Ok(())
            }
            Err(FsError::NotFound) => {
                // Create new file
                let (parent_inode, name) = self.lookup_parent(path)?;
                let mut state = self.write_state();

                // Allocate inode
                let inode_num = self.allocate_inode(&mut state, false)?;

                // Initialize inode
                let now = self.current_time();
                let mut inode = Inode {
                    type_perms: DEFAULT_FILE_PERMS,
                    uid: 0,
                    size_lower: 0,
                    access_time: now,
                    creation_time: now,
                    modification_time: now,
                    hard_links: 1,
                    ..Default::default()
                };

                // Write data
                self.write_inode_data(&mut state, inode_num, &mut inode, data)?;

                // Add directory entry
                self.add_dir_entry(&mut state, parent_inode, &name, inode_num, FT_REG_FILE)?;
                self.flush_meta(&mut state)?;

                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        if buf.is_empty() {
            return Ok(0);
        }

        let state = self.read_state();
        let inode_num = self.lookup_path_internal(&state, path)?;
        let inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        let file_size = inode.size_lower as usize;
        if offset >= file_size {
            return Ok(0);
        }

        let block_size = state.block_size;
        let end = core::cmp::min(offset + buf.len(), file_size);
        let mut total_read = 0usize;
        let mut pos = offset;

        while pos < end {
            let logical_block = (pos / block_size) as u32;
            let offset_in_block = pos % block_size;
            let chunk = core::cmp::min(block_size - offset_in_block, end - pos);

            if let Some(phys_block) = self.get_block_num(&state, &inode, logical_block)? {
                self.with_block(&state, phys_block, |b| {
                    buf[total_read..total_read + chunk]
                        .copy_from_slice(&b[offset_in_block..offset_in_block + chunk]);
                })?;
            } else {
                // Sparse block — fill with zeros
                buf[total_read..total_read + chunk].fill(0);
            }

            pos += chunk;
            total_read += chunk;
        }

        Ok(total_read)
    }

    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }

        // One write lock for resolve + optional create + data write. Avoids
        // lookup_path (read) failing with IoError while another thread holds write_state.
        let mut state = self.write_state();

        let inode_num = match self.lookup_path_internal(&state, path) {
            Ok(n) => n,
            Err(FsError::NotFound) => {
                let (parent_inode, name) = self.lookup_parent_internal(&state, path)?;
                let inode_num = self.allocate_inode(&mut state, false)?;
                let now = self.current_time();
                let mut inode = Inode {
                    type_perms: DEFAULT_FILE_PERMS,
                    uid: 0,
                    size_lower: 0,
                    access_time: now,
                    creation_time: now,
                    modification_time: now,
                    hard_links: 1,
                    ..Default::default()
                };
                self.write_inode_data(&mut state, inode_num, &mut inode, &[])?;
                self.add_dir_entry(&mut state, parent_inode, &name, inode_num, FT_REG_FILE)?;
                inode_num
            }
            Err(e) => return Err(e),
        };

        let mut inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        let block_size = state.block_size;
        let end = offset + data.len();
        let mut written = 0usize;
        let mut pos = offset;

        while pos < end {
            let logical_block = (pos / block_size) as u32;
            let offset_in_block = pos % block_size;
            let chunk = core::cmp::min(block_size - offset_in_block, end - pos);
            let full_block = offset_in_block == 0 && chunk == block_size;

            // A partial write reads the block back before patching it, so a
            // freshly allocated block there MUST be zeroed first; a full-block
            // write overwrites every byte, so it need not be.
            let phys_block =
                self.ensure_block(&mut state, &mut inode, logical_block, !full_block)?;

            if full_block {
                // Full block write — no need to read first
                let mut block_data = vec![0u8; block_size];
                block_data.copy_from_slice(&data[written..written + chunk]);
                self.write_block(&state, phys_block, &block_data)?;
            } else {
                // Partial block — read-modify-write just this one block
                let mut block_data = self.read_block(&state, phys_block)?;
                block_data[offset_in_block..offset_in_block + chunk]
                    .copy_from_slice(&data[written..written + chunk]);
                self.write_block(&state, phys_block, &block_data)?;
            }

            pos += chunk;
            written += chunk;
        }

        // Update size if we extended the file
        if end > inode.size_lower as usize {
            inode.size_lower = end as u32;
        }
        inode.modification_time = self.current_time();
        self.write_inode(&state, inode_num, &inode)?;
        self.flush_meta(&mut state)?;

        Ok(written)
    }


    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        // Check if already exists
        if self.lookup_path(path).is_ok() {
            return Err(FsError::AlreadyExists);
        }

        let (parent_inode_num, name) = self.lookup_parent(path)?;
        let mut state = self.write_state();

        // Allocate inode
        let inode_num = self.allocate_inode(&mut state, true)?;

        // Initialize directory inode
        let now = self.current_time();
        let mut inode = Inode {
            type_perms: DEFAULT_DIR_PERMS,
            uid: 0,
            size_lower: 0,
            access_time: now,
            creation_time: now,
            modification_time: now,
            hard_links: 2, // . and parent's link
            ..Default::default()
        };

        // Allocate initial block for directory entries
        let block = self.allocate_block(&mut state)?;
        inode.direct_blocks[0] = block;
        inode.size_lower = state.block_size as u32;
        inode.sectors_used = (state.block_size / 512) as u32;

        // Create . and .. entries
        let mut dir_data = vec![0u8; state.block_size];

        // . entry
        let dot_entry = DirEntryRaw {
            inode: inode_num,
            rec_len: 12,
            name_len: 1,
            file_type: FT_DIR,
        };
        dot_entry.serialize(&mut dir_data[0..DIR_ENTRY_HEADER_SIZE]);
        dir_data[DIR_ENTRY_HEADER_SIZE] = b'.';

        // .. entry
        let dotdot_entry = DirEntryRaw {
            inode: parent_inode_num,
            rec_len: (state.block_size - 12) as u16,
            name_len: 2,
            file_type: FT_DIR,
        };
        dotdot_entry.serialize(&mut dir_data[12..12 + DIR_ENTRY_HEADER_SIZE]);
        dir_data[12 + DIR_ENTRY_HEADER_SIZE] = b'.';
        dir_data[12 + DIR_ENTRY_HEADER_SIZE + 1] = b'.';

        self.write_block(&state, block, &dir_data)?;
        self.write_inode(&state, inode_num, &inode)?;

        // Update parent's hard link count
        let mut parent_inode = self.read_inode(&state, parent_inode_num)?;
        parent_inode.hard_links += 1;
        self.write_inode(&state, parent_inode_num, &parent_inode)?;

        // Add entry to parent
        self.add_dir_entry(&mut state, parent_inode_num, &name, inode_num, FT_DIR)?;
        self.flush_meta(&mut state)?;

        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        let inode_num = self.lookup_path(path)?;
        let (parent_inode, name) = self.lookup_parent(path)?;

        let mut state = self.write_state();
        let mut inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        // Retire anything whose mappings have since gone away. Doing this on the
        // unlink path is what keeps `DEFERRED_FREES` short: a build unlinks
        // constantly, so a deferred inode is normally reclaimed within
        // milliseconds of its last mapping closing.
        self.drain_deferred_frees(&mut state);

        // Remove directory entry
        self.remove_dir_entry(&mut state, parent_inode, &name)?;

        // Decrement hard link count
        inode.hard_links = inode.hard_links.saturating_sub(1);

        if inode.hard_links == 0 {
            self.release_last_link(&mut state, inode_num, &mut inode)?;
        } else {
            self.write_inode(&state, inode_num, &inode)?;
        }

        self.flush_meta(&mut state)?;
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let inode_num = self.lookup_path(path)?;

        if inode_num == ROOT_INODE {
            return Err(FsError::PermissionDenied);
        }

        let (parent_inode_num, name) = self.lookup_parent(path)?;

        let mut state = self.write_state();
        let mut inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) != S_IFDIR {
            return Err(FsError::NotADirectory);
        }

        // Check if directory is empty (only . and ..)
        let dir_data = self.read_inode_data(&state, &inode)?;
        let entries = self.parse_directory(&dir_data);
        let non_dot_entries: Vec<_> = entries
            .iter()
            .filter(|(_, n, _)| n != "." && n != "..")
            .collect();

        if !non_dot_entries.is_empty() {
            return Err(FsError::DirectoryNotEmpty);
        }

        // Remove directory entry from parent
        self.remove_dir_entry(&mut state, parent_inode_num, &name)?;

        // Update parent's hard link count
        let mut parent_inode = self.read_inode(&state, parent_inode_num)?;
        parent_inode.hard_links = parent_inode.hard_links.saturating_sub(1);
        self.write_inode(&state, parent_inode_num, &parent_inode)?;

        // Free blocks
        self.truncate_inode(&mut state, &mut inode)?;
        // A directory carries `hard_links = 2` while it exists (`.` plus the
        // parent's entry). Both are gone now, so the record must say so before
        // the number returns to the allocator: `e2fsck` reads a `links_count`
        // that survives a set `dtime` as corruption ("in use, but has dtime
        // set"), and every `rmdir` used to leave one behind — 15 of them in a
        // single `ext2probe-host` run, reproducible on the host after an
        // explicit `sync()`. `unlink` never had the bug because its caller
        // decrements to zero before `release_last_link` writes the inode
        // (see `Self::unlink`); this path only ever decremented the *parent's*
        // count. `docs/archive/BKL_VFS_CARVE_OUT.md` §12.4 attributed the same
        // signature to a dirty bitmap block lost to a `kill`; that reading is
        // superseded — see `docs/archive/AKUMA_EXT2_CLEANUP.md` §6.1.
        inode.hard_links = 0;
        inode.deletion_time = self.current_time();
        self.write_inode(&state, inode_num, &inode)?;

        // Free inode
        self.free_inode(&mut state, inode_num, true)?;

        self.flush_meta(&mut state)?;
        Ok(())
    }

    fn create_symlink(&self, link_path: &str, target: &str) -> Result<(), FsError> {
        if self.lookup_path(link_path).is_ok() {
            return Err(FsError::AlreadyExists);
        }
        let (parent_inode, name) = self.lookup_parent(link_path)?;
        let mut state = self.write_state();
        self.create_symlink_internal(&mut state, parent_inode, &name, target)?;
        self.flush_meta(&mut state)?;
        Ok(())
    }

    fn create_socket_node(&self, path: &str) -> Result<(), FsError> {
        // `AlreadyExists` is the honest answer and the caller depends on it:
        // AF_UNIX `bind` maps it to `EADDRINUSE`, which is what tells a daemon
        // it must `unlink` a stale node before it can restart. Silently reusing
        // the node would let two daemons believe they own the same path.
        if self.lookup_path(path).is_ok() {
            return Err(FsError::AlreadyExists);
        }
        let (parent_inode, name) = self.lookup_parent(path)?;
        let mut state = self.write_state();
        self.create_socket_node_internal(&mut state, parent_inode, &name)?;
        self.flush_meta(&mut state)?;
        Ok(())
    }

    fn read_symlink(&self, path: &str) -> Result<String, FsError> {
        let state = self.read_state();
        let inode_num = self.lookup_path_internal(&state, path)?;
        self.read_symlink_inode(&state, inode_num)
    }

    fn is_symlink(&self, path: &str) -> bool {
        let state = self.read_state();
        if let Ok(inode_num) = self.lookup_path_internal(&state, path) {
            if let Ok(inode) = self.read_inode(&state, inode_num) {
                return (inode.type_perms & 0xF000) == S_IFLNK;
            }
        }
        false
    }

    fn rename(&self, old_path: &str, new_path: &str) -> Result<(), FsError> {
        let src_inode_num = self.lookup_path(old_path)?;
        let (src_parent, src_name) = self.lookup_parent(old_path)?;
        let (dst_parent, dst_name) = self.lookup_parent(new_path)?;

        let mut state = self.write_state();
        let src_inode = self.read_inode(&state, src_inode_num)?;
        let ft = if (src_inode.type_perms & 0xF000) == S_IFDIR { FT_DIR } else { FT_REG_FILE };

        // Retire anything whose readers have since gone away, for the reason
        // `remove_file` does it: renaming over a destination is an unlink, and a
        // build renames constantly, so this is where the deferral list drains.
        self.drain_deferred_frees(&mut state);

        // If destination exists, remove it first
        if let Ok(dst_inode_num) = self.lookup_path_internal(&state, new_path) {
            // POSIX: if the two names already resolve to the same inode — the
            // same path twice, or two hard links to one file — `rename` does
            // nothing and succeeds. Without this the code below unlinks that
            // single shared inode, drops its last link, frees it, and then
            // re-adds a directory entry pointing at the freed number: `mv a a`
            // destroyed the file and left a dangling entry behind.
            if dst_inode_num == src_inode_num {
                return Ok(());
            }
            let mut dst_inode = self.read_inode(&state, dst_inode_num)?;
            self.remove_dir_entry(&mut state, dst_parent, &dst_name)?;
            dst_inode.hard_links = dst_inode.hard_links.saturating_sub(1);
            if dst_inode.hard_links == 0 {
                self.release_last_link(&mut state, dst_inode_num, &mut dst_inode)?;
            } else {
                self.write_inode(&state, dst_inode_num, &dst_inode)?;
            }
        }

        self.add_dir_entry(&mut state, dst_parent, &dst_name, src_inode_num, ft)?;
        self.remove_dir_entry(&mut state, src_parent, &src_name)?;
        self.flush_meta(&mut state)?;

        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        self.lookup_path(path).is_ok()
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        let state = self.read_state();
        let inode_num = self.lookup_path_internal(&state, path)?;
        self.metadata_of(&state, inode_num)
    }

    fn metadata_by_inode(&self, inode_num: u32) -> Result<Metadata, FsError> {
        let state = self.read_state();
        self.metadata_of(&state, inode_num)
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), FsError> {
        let inode_num = self.lookup_path(path)?;
        let state = self.write_state();
        let mut inode = self.read_inode(&state, inode_num)?;
        inode.type_perms = (inode.type_perms & 0xF000) | (mode as u16 & 0o7777);
        inode.modification_time = self.current_time();
        self.write_inode(&state, inode_num, &inode)?;
        Ok(())
    }

    /// `utimensat` backing. Modelled on `chmod` directly above — the inode
    /// already carries all three stamps and `write_inode` already persists them,
    /// so this is the plumbing that was missing rather than new capability.
    ///
    /// Deliberately does NOT touch `modification_time` when only `atime_secs` is
    /// given: `chmod` bumps mtime because changing the mode is a change to the
    /// inode, but `touch -a` must leave mtime exactly as it was — that is the
    /// whole point of `UTIME_OMIT`. Times narrow to the on-disk `u32` (ext2 is a
    /// 2038 format); saturating rather than wrapping so a far-future stamp pins
    /// at the maximum instead of silently becoming 1970.
    fn set_times(
        &self,
        path: &str,
        atime_secs: Option<u64>,
        mtime_secs: Option<u64>,
    ) -> Result<(), FsError> {
        if atime_secs.is_none() && mtime_secs.is_none() {
            return Ok(());
        }
        let inode_num = self.lookup_path(path)?;
        let state = self.write_state();
        let mut inode = self.read_inode(&state, inode_num)?;
        if let Some(a) = atime_secs {
            inode.access_time = u32::try_from(a).unwrap_or(u32::MAX);
        }
        if let Some(m) = mtime_secs {
            inode.modification_time = u32::try_from(m).unwrap_or(u32::MAX);
        }
        self.write_inode(&state, inode_num, &inode)?;
        Ok(())
    }

    fn fallocate(&self, path: &str, mode: i32, offset: u64, len: u64) -> Result<(), FsError> {
        if mode != 0 {
            return Err(FsError::NotSupported);
        }
        if len == 0 {
            return Ok(());
        }

        let inode_num = self.lookup_path(path)?;
        let mut state = self.write_state();
        let mut inode = self.read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) != S_IFREG {
            return Err(FsError::NotAFile);
        }

        let block_size = state.block_size as u64;
        let first_block = offset / block_size;
        let last_block = (offset + len - 1) / block_size;

        for lb in first_block..=last_block {
            // fallocate writes no data — the preallocated blocks must read as zero.
            self.ensure_block(&mut state, &mut inode, lb as u32, true)?;
        }

        let end = offset + len;
        let current_size = inode.size_lower as u64 | ((inode.size_upper as u64) << 32);
        if end > current_size {
            inode.size_lower = end as u32;
            inode.size_upper = (end >> 32) as u32;
        }
        inode.modification_time = self.current_time();
        self.write_inode(&state, inode_num, &inode)?;
        self.flush_meta(&mut state)?;
        Ok(())
    }

    fn truncate(&self, path: &str, length: u64) -> Result<(), FsError> {
        let state = self.read_state();
        let inode_num = self.lookup_path_internal(&state, path)?;
        let mut inode = self.read_inode(&state, inode_num)?;
        
        // Only allow truncate on regular files
        if inode.type_perms & 0x8000 == 0 {
            return Err(FsError::NotAFile);
        }
        
        // For now, only support truncating to existing size or smaller
        // (shrinking doesn't need to allocate new blocks)
        let current_size = inode.size_lower as u64 | ((inode.size_upper as u64) << 32);
        if length > current_size {
            // Extending would require allocating blocks - not implemented
            // For bun's use case, this is fine (it truncates to shrink)
            return Ok(());
        }
        
        inode.size_lower = length as u32;
        inode.size_upper = (length >> 32) as u32;
        inode.modification_time = self.current_time();
        self.write_inode(&state, inode_num, &inode)?;
        Ok(())
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        let state = self.read_state();
        let total_blocks = state.superblock.total_blocks;
        let unallocated_blocks = state.superblock.unallocated_blocks;

        Ok(FsStats {
            block_size: state.block_size as u32,
            total_blocks: total_blocks as u64,
            free_blocks: unallocated_blocks as u64,
        })
    }

    fn sync(&self) -> Result<(), FsError> {
        // Flush any superblock / block-group-descriptor free-count updates that
        // `stage_*` deferred. Every mutating method already flushes its own, so
        // this normally has nothing to do; it is the backstop for unmount and
        // for an explicit `fsync`/`sync` syscall. Data + inode + bitmap blocks
        // are always written through, so there is nothing else to push.
        let mut state = self.write_state();
        self.flush_meta(&mut state)
    }

    fn resolve_inode(&self, path: &str) -> Result<u32, FsError> {
        self.lookup_path(path)
    }

    fn read_at_by_inode(&self, inode_num: u32, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        Ext2Filesystem::read_at_by_inode(self, inode_num, offset, buf)
    }
}

// ============================================================================
