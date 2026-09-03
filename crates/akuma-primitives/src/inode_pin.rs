//! Inode pins — keeping a file's data alive for the lifetime of a mapping.
//!
//! # The defect this exists to close
//!
//! A `LazySource::File` region names its backing file by **raw inode number**
//! plus the `filesz` captured at mmap time, and holds no reference on the file.
//! Linux pins the inode through the mapping's `struct file`; this kernel dropped
//! every reference at mmap time. So `unlink` was free to truncate the inode and
//! hand its number to the next file created, while a live mapping still named
//! it. The mapper's next fault then read the freed inode and got either
//!
//! - `i_size == 0` → `read_at_by_inode` returns `Ok(0)` → the page stays zero, or
//! - after the number was reused → **another file's bytes**.
//!
//! Both were silent. They are the `[FILL-SHORT] got=Ok(0)` flood and the
//! garbage-byte metadata decode failures of the self-host `rustc` ICE hunt —
//! root cause #2, `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14.
//!
//! # The shape of the fix
//!
//! A mapping holds an [`InodePin`] for as long as it exists, and the filesystem
//! refuses to free a pinned inode — it unlinks the name, defers the truncate and
//! the bitmap free, and completes them when the last pin drops. That is Linux's
//! "unlinked but still open" semantics, reached from the mapping side because
//! this kernel has no open-file object to hang it on.
//!
//! [`InodePin`] is the whole mechanism: its `Clone` increments and its `Drop`
//! decrements, so every path that copies or destroys a region — `push`,
//! `remove`, `clear`, fork's `extend_from_slice`/`replace_with_clone`,
//! `update_flags`'s split/reinsert, `munmap_one_overlap`'s four clip shapes, and
//! `Process::drop` — stays balanced **by construction**. No call site maintains
//! the count by hand, because the sites that would have to are exactly the ones
//! that have historically been missed.
//!
//! # Why lock-free
//!
//! The two callers sit on opposite sides of a lock-ordering hazard: the pin is
//! taken and dropped on the demand-fault path, and `is_pinned` is read by ext2
//! *while holding its state write lock*. A spinlock here would be an AB-BA
//! waiting to happen, so the table is a fixed open-addressed array of atomics
//! with CAS updates — no allocation, no lock, callable from any context.
//!
//! # Conservative by design
//!
//! Every failure mode defers a free rather than permitting one:
//!
//! - The table is keyed on the inode number **alone**, with no filesystem
//!   identity. Two mounts sharing a number therefore alias — which can only
//!   *add* pins, never drop one, so it costs a deferred free and never a freed
//!   mapping.
//! - If the table has no room, the pin is not recorded and the overflow counter
//!   for that inode's **region** counts it (see [`REGIONS`]). While that
//!   region's count is non-zero [`is_pinned`] answers `true` for every inode in
//!   it, because an unrecorded pin is indistinguishable from any other inode
//!   hashing there. It self-clears: an unpin that finds no entry decrements it
//!   again. Accounting per region rather than globally is deliberate — one lost
//!   pin used to make *every* inode read pinned, which stalls a deferred-free
//!   drain into an unbounded queue.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Slots in the pin table. Sized far above the live set — a `-j4` kernel build
/// maps on the order of tens of file-backed inodes at once — because the cost of
/// running out is [`OVERFLOW`]'s blunt "nothing may be freed" mode.
const SLOTS: usize = 1024;
const MASK: usize = SLOTS - 1;

/// How far a lookup probes before declaring the table full. Chains only reach
/// this length when the table is near capacity; a miss walks to the first empty
/// slot, which is O(1) at any sane load factor.
const PROBE_LIMIT: usize = 32;

/// `(inode << 32) | count`. A slot is empty iff it is `0`, which is
/// unambiguous because inode 0 is never pinned (it is the "no inode, read by
/// path" sentinel in `LazySource::File`).
///
/// `count == 0` with a non-zero inode is a **tombstone**: the key stays so it
/// cannot break the probe chain of any key that hashed before it, and the slot
/// is reused by the next insert that passes it.
static TABLE: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// Live pins that did not fit in [`TABLE`]. Non-zero makes [`is_pinned`] answer
/// `true` for every inode — see the module header.
static OVERFLOW: [AtomicUsize; REGIONS] = [const { AtomicUsize::new(0) }; REGIONS];

/// Regions the table is divided into for overflow accounting. An unrecorded pin
/// makes only its **own** region answer conservatively, instead of the whole
/// table.
///
/// Why this exists: a single global flag meant one lost pin made
/// [`is_pinned`] answer `true` for *every* inode, which stops
/// `akuma_ext2`'s `drain_deferred_frees` from freeing anything at all — so its
/// deferral queue climbs instead of draining and eventually leaks. Measured over
/// four in-guest builds: `pin_ovf` hovering at 3-27 (never 0) took `defer` from
/// 0 to 2252 monotonically, 55 % of its bound
/// (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.4). `pin_ovf` does not need to be
/// large to block the drain completely — only non-zero.
///
/// 64 regions of 16 slots cuts the blast radius of one lost pin from 1/1 to
/// ~1/64, so the drain keeps making progress on the other 63 regions.
const REGIONS: usize = 64;
const SLOTS_PER_REGION: usize = SLOTS / REGIONS;

/// The overflow region an inode is accounted to: the region of its **home**
/// slot.
///
/// Home, not wherever the probe ended up — and that is what makes it sound. A
/// probe chain can spill past a region boundary, but `acquire`, `release` and
/// `is_pinned` all derive the region from the same `slot_of(inode)`, so each
/// inode's conservative answer is governed by its own home region and by nothing
/// else. An inode whose pin was lost therefore still reads pinned; an inode in
/// another region is unaffected.
const fn region_of(inode: u32) -> usize {
    slot_of(inode) / SLOTS_PER_REGION
}

/// Total unrecorded pins across every region — diagnostics only
/// (`[INODE] pin_ovf=`). Non-zero means at least one region is answering
/// conservatively.
#[must_use]
pub fn overflow_count() -> usize {
    let mut n = 0;
    for r in &OVERFLOW {
        n += r.load(Ordering::Acquire);
    }
    n
}

const fn pack(inode: u32, count: u32) -> u64 {
    ((inode as u64) << 32) | count as u64
}

const fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, (v & 0xFFFF_FFFF) as u32)
}

/// Fibonacci hashing — inode numbers are dense and sequential, so the low bits
/// alone would cluster every artifact of one build into one probe chain.
const fn slot_of(inode: u32) -> usize {
    (inode.wrapping_mul(0x9E37_79B9) >> 16) as usize & MASK
}

/// A reference that keeps `inode`'s data alive. Clone to share, drop to release.
///
/// Deliberately **not** `Copy`: the count is maintained entirely by `Clone` and
/// `Drop`, and a `Copy` type would duplicate pins without incrementing.
#[derive(Debug)]
pub struct InodePin {
    inode: u32,
}

impl InodePin {
    /// Pin `inode`. `inode == 0` yields an inert handle — that value means "this
    /// region reads by path, not by inode", so there is nothing to keep alive.
    #[must_use]
    pub fn new(inode: u32) -> Self {
        if inode != 0 {
            acquire(inode);
        }
        Self { inode }
    }

    /// An inert pin, for regions with no inode identity at all.
    #[must_use]
    pub const fn none() -> Self {
        Self { inode: 0 }
    }

    #[must_use]
    pub const fn inode(&self) -> u32 {
        self.inode
    }
}

impl Clone for InodePin {
    fn clone(&self) -> Self {
        Self::new(self.inode)
    }
}

impl Drop for InodePin {
    fn drop(&mut self) {
        if self.inode != 0 {
            release(self.inode);
        }
    }
}

/// Take a reference on `inode`, inserting it if this is the first.
fn acquire(inode: u32) {
    let home = slot_of(inode);
    'restart: loop {
        // First tombstone passed while looking for the key. Reused only once the
        // probe proves the key is absent, so an existing entry always wins.
        let mut tomb: Option<(usize, u64)> = None;
        for i in 0..PROBE_LIMIT {
            let pos = (home + i) & MASK;
            let cur = TABLE[pos].load(Ordering::Acquire);
            if cur == 0 {
                // Chain ends here, so the key is absent. Prefer a tombstone
                // passed on the way, else claim this empty slot.
                let (target, expect) = tomb.unwrap_or((pos, 0));
                if TABLE[target]
                    .compare_exchange(expect, pack(inode, 1), Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
                // Lost the slot to a concurrent insert; the chain may now hold
                // our key, so re-probe from the top rather than pressing on.
                continue 'restart;
            }
            let (ino, cnt) = unpack(cur);
            if ino == inode {
                if TABLE[pos]
                    .compare_exchange(
                        cur,
                        pack(ino, cnt.saturating_add(1)),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return;
                }
                continue 'restart;
            }
            if cnt == 0 && tomb.is_none() {
                tomb = Some((pos, cur));
            }
        }
        // The chain ran to PROBE_LIMIT without an empty slot. Before declaring
        // overflow, claim a TOMBSTONE if the walk passed one.
        //
        // This is the fix for the pin table overflowing on a table that is ~0.5 %
        // *live*. `release` leaves `(inode, 0)` behind rather than clearing the
        // slot — necessary, because zeroing a slot mid-chain would truncate the
        // probe chain of any key that hashed earlier and probed past it. Those
        // tombstones are what `tomb` exists to recycle, but it was only consulted
        // inside the `cur == 0` branch above: a window of 32 slots that are all
        // live-or-tombstone never reaches `cur == 0`, so it fell through to here
        // and reported overflow **while holding a reusable slot it had just
        // walked past**.
        //
        // A build maps and unmaps thousands of files through a 1024-slot table,
        // so it saturates with tombstones within one build and then overflowed on
        // essentially every `acquire`. That kept `pin_ovf` permanently non-zero
        // with `pin` reading 5-93, which stalls `akuma_ext2`'s deferred-free
        // drain and takes its queue to the bound
        // (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.5).
        if let Some((target, expect)) = tomb {
            if TABLE[target]
                .compare_exchange(expect, pack(inode, 1), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
            // Lost the tombstone to a concurrent insert. Re-probe: the chain may
            // now hold our key, and there may be another tombstone further on.
            continue 'restart;
        }
        OVERFLOW[region_of(inode)].fetch_add(1, Ordering::AcqRel);
        return;
    }
}

/// Drop a reference on `inode`, tombstoning the slot when the last one goes.
fn release(inode: u32) {
    let home = slot_of(inode);
    'restart: loop {
        for i in 0..PROBE_LIMIT {
            let pos = (home + i) & MASK;
            let cur = TABLE[pos].load(Ordering::Acquire);
            if cur == 0 {
                break;
            }
            let (ino, cnt) = unpack(cur);
            if ino == inode {
                if cnt == 0 {
                    // Already at zero: this unpin has no matching entry, so it
                    // belongs to a pin lost to overflow. Balance that instead.
                    break;
                }
                if TABLE[pos]
                    .compare_exchange(cur, pack(ino, cnt - 1), Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    if cnt == 1 {
                        // This slot just became a tombstone. Compact it away if we
                        // can, because unbounded tombstone growth is what breaks
                        // this table — see `compact_tail`.
                        compact_tail(pos);
                    }
                    return;
                }
                continue 'restart;
            }
        }
        // No entry: the matching `acquire` overflowed. Cancel it out so the
        // table's conservative mode ends when the lost pins are gone.
        let slot = &OVERFLOW[region_of(inode)];
        let mut cur = slot.load(Ordering::Acquire);
        while cur > 0 {
            match slot.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(v) => cur = v,
            }
        }
        return;
    }
}

/// Clear the tombstone at `pos`, and any run of tombstones behind it, **iff
/// doing so cannot truncate a probe chain.**
///
/// # Why this is necessary
///
/// `release` leaves `(inode, 0)` rather than `0`, because zeroing a slot
/// mid-chain would truncate the probe chain of every key that hashed earlier and
/// probed past it. Left to accumulate, those tombstones break the table in *two*
/// separate places, and the second one is subtle:
///
/// - `acquire` walks `PROBE_LIMIT` slots without finding an empty one and reports
///   overflow (fixed separately by recycling the tombstone it passed), and
/// - **`is_pinned` never sees `cur == 0`, so it falls through its probe loop to
///   its conservative `true`.** That makes it answer "pinned" for *every* inode
///   with zero live pins and no overflow, which stops `akuma_ext2`'s
///   `drain_deferred_frees` from freeing anything at all. Measured: 4567 drain
///   calls, **0 inodes freed**, `slots=1015/1024` occupied against `pin=0` live
///   (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.6).
///
/// # Why it is sound
///
/// A slot is cleared only when the **next** slot is already empty. Linear-probe
/// chains are contiguous and terminate at the first empty slot, so if `pos + 1`
/// is empty then no key whose home is at or before `pos` can live beyond `pos` —
/// there is no chain passing *through* `pos` to truncate.
///
/// The race is closed by using `compare_exchange` rather than `store`: for a key
/// with home at or before `pos` to land at `pos + 1`, `acquire` would have to walk
/// past this tombstone, and it cannot — it records the first tombstone it passes
/// and claims it once it reaches the empty slot, so it takes `pos` itself. Our CAS
/// then fails and we leave the slot alone, which is correct.
fn compact_tail(pos: usize) {
    let mut at = pos;
    for _ in 0..PROBE_LIMIT {
        // Only safe while the following slot is empty.
        if TABLE[(at + 1) & MASK].load(Ordering::Acquire) != 0 {
            return;
        }
        let cur = TABLE[at].load(Ordering::Acquire);
        if cur == 0 {
            return;
        }
        let (_, cnt) = unpack(cur);
        if cnt != 0 {
            // A live entry, not a tombstone. Stop: it must stay.
            return;
        }
        if TABLE[at]
            .compare_exchange(cur, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Someone claimed or revived it; nothing to compact here.
            return;
        }
        // Clearing `at` may have exposed a tombstone behind it. Walk back.
        at = at.wrapping_sub(1) & MASK;
    }
}

/// Does a live mapping name `inode`?
///
/// The filesystem asks this before freeing an inode, so **`true` must be
/// returned whenever the answer is not certainly `false`**: a wrong `true` costs
/// a deferred free, a wrong `false` costs a corrupted mapping.
#[must_use]
pub fn is_pinned(inode: u32) -> bool {
    if inode == 0 {
        return false;
    }
    // Only THIS inode's region, not the whole table: a pin lost in some other
    // region says nothing about this inode, and answering `true` for everything
    // is what stalls `drain_deferred_frees` into an unbounded queue.
    if OVERFLOW[region_of(inode)].load(Ordering::Acquire) > 0 {
        return true;
    }
    let home = slot_of(inode);
    for i in 0..PROBE_LIMIT {
        let pos = (home + i) & MASK;
        let cur = TABLE[pos].load(Ordering::Acquire);
        if cur == 0 {
            return false;
        }
        let (ino, cnt) = unpack(cur);
        if ino == inode {
            return cnt > 0;
        }
    }
    // The chain ran to the probe limit without an empty slot, so the table is
    // congested and absence cannot be proven. Say pinned.
    true
}

/// Slots holding anything at all — live pins **and tombstones** — out of
/// [`SLOTS`].
///
/// The companion [`pinned_inodes`] counts only `cnt > 0`, which makes tombstones
/// invisible, and that is exactly how a table saturated with dead entries hid
/// behind a `pin=5` reading while `acquire` spuriously reported overflow on
/// every insert (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.5). A rising figure here
/// with a low `pinned_inodes` is the tombstone-congestion signature; if it sits
/// near [`SLOTS`], probe chains are long and inserts are recycling tombstones
/// rather than finding empty slots.
#[must_use]
pub fn slots_occupied() -> usize {
    TABLE
        .iter()
        .filter(|s| s.load(Ordering::Relaxed) != 0)
        .count()
}

/// Live pin count for `inode` — diagnostics and tests only. Does **not** consult
/// the overflow counters, so it reports what the table actually holds.
#[must_use]
pub fn pin_count(inode: u32) -> u32 {
    if inode == 0 {
        return 0;
    }
    let home = slot_of(inode);
    for i in 0..PROBE_LIMIT {
        let pos = (home + i) & MASK;
        let cur = TABLE[pos].load(Ordering::Acquire);
        if cur == 0 {
            return 0;
        }
        let (ino, cnt) = unpack(cur);
        if ino == inode {
            return cnt;
        }
    }
    0
}

/// Number of distinct inodes with at least one live pin, for the `[Mem]` dump.
#[must_use]
pub fn pinned_inodes() -> usize {
    TABLE
        .iter()
        .filter(|s| {
            let (ino, cnt) = unpack(s.load(Ordering::Relaxed));
            ino != 0 && cnt > 0
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The table is a process-wide static, so tests must not share inode numbers —
    // each picks its own disjoint range. That is not enough on its own: the
    // overflow test drives `OVERFLOW` above zero, which makes `is_pinned` answer
    // `true` for *every* inode by design, so a peer test asserting "not pinned"
    // would fail if it ran alongside. Serialize them rather than weaken the
    // assertions, since the conservative answer is the property under test.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn pin_holds_and_releases() {
        let _g = serial();
        let ino = 10_001;
        assert!(!is_pinned(ino));
        let p = InodePin::new(ino);
        assert!(is_pinned(ino));
        assert_eq!(pin_count(ino), 1);
        drop(p);
        assert!(!is_pinned(ino));
        assert_eq!(pin_count(ino), 0);
    }

    #[test]
    fn clone_adds_a_reference_and_drop_removes_one() {
        let _g = serial();
        let ino = 10_002;
        let a = InodePin::new(ino);
        let b = a.clone();
        let c = b.clone();
        assert_eq!(pin_count(ino), 3);
        drop(b);
        assert_eq!(pin_count(ino), 2);
        drop(a);
        assert!(is_pinned(ino), "still one live mapping");
        drop(c);
        assert!(!is_pinned(ino), "last mapping gone");
    }

    #[test]
    fn inode_zero_is_never_pinned() {
        let _g = serial();
        let p = InodePin::new(0);
        assert_eq!(p.inode(), 0);
        assert!(!is_pinned(0));
        assert_eq!(pin_count(0), 0);
        drop(p);
        assert!(!is_pinned(0));
    }

    #[test]
    // The clone is the subject of the test, not a copy made out of laziness:
    // cloning an inert pin must stay inert rather than acquiring inode 0.
    #[allow(clippy::redundant_clone)]
    fn none_is_inert() {
        let _g = serial();
        let p = InodePin::none();
        assert_eq!(p.inode(), 0);
        let q = p.clone();
        assert_eq!(q.inode(), 0);
        assert!(!is_pinned(0));
    }

    #[test]
    fn slot_is_reused_after_the_last_pin_drops() {
        let _g = serial();
        // A tombstone must not leak a slot: cycling one inode many times more
        // than the table has slots would exhaust it if it did.
        let ino = 10_003;
        for _ in 0..(SLOTS * 4) {
            let p = InodePin::new(ino);
            drop(p);
        }
        assert_eq!(overflow_count(), 0, "no overflow from reuse");
        assert!(!is_pinned(ino));
    }

    #[test]
    fn distinct_inodes_coexist() {
        let _g = serial();
        let base = 20_000;
        let pins: Vec<InodePin> = (0..64).map(|i| InodePin::new(base + i)).collect();
        for i in 0..64 {
            assert!(is_pinned(base + i), "inode {} lost its pin", base + i);
        }
        assert!(!is_pinned(base + 64), "unpinned neighbour must stay free");
        drop(pins);
        for i in 0..64 {
            assert!(!is_pinned(base + i));
        }
    }

    #[test]
    fn colliding_inodes_do_not_shadow_each_other() {
        let _g = serial();
        // Two inodes whose home slots collide must still be tracked apart.
        let a = 30_001;
        let mut b = a + 1;
        while slot_of(b) != slot_of(a) {
            b += 1;
            assert!(b < a + 1_000_000, "no colliding pair found");
        }
        let pa = InodePin::new(a);
        assert!(is_pinned(a));
        assert!(!is_pinned(b), "collision must not imply a pin");
        let pb = InodePin::new(b);
        drop(pa);
        assert!(!is_pinned(a), "dropping a must not free b's slot state");
        assert!(is_pinned(b));
        drop(pb);
        assert!(!is_pinned(b));
    }

    /// **The bug that stopped the ext2 deferred-free drain.** After heavy
    /// pin/unpin churn the table must not report a never-pinned inode as pinned,
    /// and must not stay occupied.
    ///
    /// `is_pinned` terminates its probe on `cur == 0`; a tombstone is non-zero,
    /// so a window saturated with tombstones made it fall through to its
    /// conservative `true` — for **every** inode, with zero live pins and no
    /// overflow. `drain_deferred_frees` re-asks `is_pinned` per entry, so it froze
    /// completely: measured 4567 drain calls and **0** inodes freed, with
    /// `slots=1015/1024` occupied against `pin=0`
    /// (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.6).
    #[test]
    fn churn_does_not_leave_the_table_answering_pinned_for_everything() {
        let _g = serial();
        let home_seed = 90_001;
        let home = slot_of(home_seed);
        let mut colliders = Vec::new();
        let mut cand = home_seed;
        while colliders.len() < PROBE_LIMIT + 8 {
            if slot_of(cand) == home {
                colliders.push(cand);
            }
            cand += 1;
            assert!(cand < home_seed + 100_000_000, "not enough colliders");
        }

        // Saturate the window, then release everything: pure churn, nothing live.
        let fill: Vec<InodePin> = colliders.iter().map(|i| InodePin::new(*i)).collect();
        drop(fill);

        // An inode on that chain that was NEVER pinned must read unpinned. This is
        // the question `drain_deferred_frees` asks about every queued entry.
        let never = {
            let mut c = cand;
            loop {
                if slot_of(c) == home && !colliders.contains(&c) {
                    break c;
                }
                c += 1;
                assert!(c < cand + 100_000_000, "no further collider");
            }
        };
        assert_eq!(pin_count(never), 0, "fixture: it must never have been pinned");
        assert_eq!(overflow_count(), 0, "fixture: no overflow should be involved");
        assert!(
            !is_pinned(never),
            "a table churned to saturation must not answer `pinned` for an inode \
             that was never pinned — this is what froze the ext2 drain at 0 frees",
        );

        // And the tombstones must actually be gone, not merely tolerated: an
        // unbounded occupancy is what produces the saturation in the first place.
        assert!(
            slots_occupied() < PROBE_LIMIT,
            "released pins must be compacted away; {} slots still occupied",
            slots_occupied(),
        );
    }

    /// A chain saturated with **tombstones** — dead entries, no live pins — must
    /// not report overflow. Recycling them is what tombstones are for.
    ///
    /// This is the defect that kept `pin_ovf` permanently non-zero on a table
    /// only ~0.5 % live: `acquire` recorded the first tombstone it passed but
    /// only *used* it inside its `cur == 0` branch, so a 32-slot window with no
    /// empty slot fell through to the overflow counter while holding a reusable
    /// slot. A build cycles thousands of inodes through 1024 slots, saturating
    /// them with tombstones within one build, after which nearly every `acquire`
    /// overflowed — which stalls `akuma_ext2`'s deferred-free drain and drives
    /// its queue to the bound (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.5).
    #[test]
    fn a_chain_of_tombstones_is_recycled_not_overflowed() {
        let _g = serial();
        // Colliders: enough to fill a probe window and then some.
        let home_seed = 70_001;
        let home = slot_of(home_seed);
        let mut colliders = Vec::new();
        let mut cand = home_seed;
        while colliders.len() < PROBE_LIMIT + 4 {
            if slot_of(cand) == home {
                colliders.push(cand);
            }
            cand += 1;
            assert!(cand < home_seed + 100_000_000, "not enough colliders");
        }

        // Fill the chain, then release everything: every slot in the window is
        // now `(inode, 0)` — a tombstone — and nothing is live.
        //
        // The pins must be held **simultaneously** and dropped together. A
        // first attempt at this fixture did `drop(InodePin::new(i))` per inode,
        // which acquires and releases one at a time — so each insert recycled
        // the *previous* tombstone, only one slot was ever occupied, and the
        // test passed with the fix removed. Verified by re-running it against
        // the unfixed `acquire`.
        let fill: Vec<InodePin> = colliders.iter().map(|i| InodePin::new(*i)).collect();
        drop(fill);
        assert_eq!(
            pin_count(colliders[0]), 0,
            "the fixture must leave no live pins",
        );

        let before = overflow_count();
        // A fresh inode on that same chain must find a home in a recycled
        // tombstone rather than reporting overflow.
        let newcomer = colliders[colliders.len() - 1] + 0; // same chain by construction
        let fresh = {
            let mut c = newcomer + 1;
            loop {
                if slot_of(c) == home && !colliders.contains(&c) {
                    break c;
                }
                c += 1;
                assert!(c < newcomer + 100_000_000, "no further collider");
            }
        };
        let pin = InodePin::new(fresh);
        assert_eq!(
            overflow_count(), before,
            "a chain of tombstones must be recycled, not reported as overflow",
        );
        assert_eq!(pin_count(fresh), 1, "the new pin must be recorded");
        assert!(is_pinned(fresh));
        drop(pin);
    }

    #[test]
    fn overflow_is_conservative_and_self_clearing() {
        let _g = serial();
        // Drive a single probe chain past PROBE_LIMIT by pinning inodes that all
        // hash to the same home slot.
        let home_seed = 40_001;
        let home = slot_of(home_seed);
        let mut colliders = Vec::new();
        let mut cand = home_seed;
        while colliders.len() < PROBE_LIMIT + 2 {
            if slot_of(cand) == home {
                colliders.push(cand);
            }
            cand += 1;
            assert!(cand < home_seed + 100_000_000, "not enough colliders");
        }
        let before = overflow_count();
        let pins: Vec<InodePin> = colliders.iter().map(|i| InodePin::new(*i)).collect();
        assert!(
            overflow_count() > before,
            "a full chain must record overflow"
        );
        // Conservative WITHIN the overflowed region: an inode hashing there reads
        // pinned even though nothing recorded a pin for it. This is the property
        // that keeps a lost pin from ever permitting a free.
        let victim = (home_seed..home_seed + 10_000_000)
            .find(|i| region_of(*i) == region_of(home_seed) && !colliders.contains(i))
            .expect("an unpinned inode in the overflowed region");
        assert!(
            is_pinned(victim),
            "an inode in the overflowed region must answer conservatively",
        );

        // ...but CONFINED to it: an inode in another region is unaffected. This
        // is the whole point of per-region accounting — with one global flag this
        // read was `true`, which stalls a deferred-free drain into an unbounded
        // queue (`EXT2_UNLINK_INODE_BLOCK_LEAK.md` §2.4).
        let elsewhere = (1u32..10_000_000)
            .find(|i| region_of(*i) != region_of(home_seed) && pin_count(*i) == 0)
            .expect("an inode in a different region");
        assert!(
            !is_pinned(elsewhere),
            "overflow in one region must NOT make inode {elsewhere} (region {}) read pinned; \
             a global flag here is what blocked the ext2 deferred-free drain",
            region_of(elsewhere),
        );

        drop(pins);
        assert_eq!(
            overflow_count(),
            before,
            "overflow must unwind as the lost pins are released"
        );
    }
}
