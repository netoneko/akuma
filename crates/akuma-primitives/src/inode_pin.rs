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
//! - If the table has no room, the pin is not recorded and [`OVERFLOW`] counts
//!   it. While that count is non-zero [`is_pinned`] answers `true` for
//!   everything, because an unrecorded pin is indistinguishable from any other
//!   inode. It self-clears: an unpin that finds no entry decrements it again.

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
pub static OVERFLOW: AtomicUsize = AtomicUsize::new(0);

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
        OVERFLOW.fetch_add(1, Ordering::AcqRel);
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
                    return;
                }
                continue 'restart;
            }
        }
        // No entry: the matching `acquire` overflowed. Cancel it out so the
        // table's conservative mode ends when the lost pins are gone.
        let mut cur = OVERFLOW.load(Ordering::Acquire);
        while cur > 0 {
            match OVERFLOW.compare_exchange_weak(
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
    if OVERFLOW.load(Ordering::Acquire) > 0 {
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

/// Live pin count for `inode` — diagnostics and tests only. Does **not** consult
/// [`OVERFLOW`], so it reports what the table actually holds.
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
        assert_eq!(OVERFLOW.load(Ordering::Acquire), 0, "no overflow from reuse");
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
        let before = OVERFLOW.load(Ordering::Acquire);
        let pins: Vec<InodePin> = colliders.iter().map(|i| InodePin::new(*i)).collect();
        assert!(
            OVERFLOW.load(Ordering::Acquire) > before,
            "a full chain must record overflow"
        );
        // While pins are lost, every inode reads as pinned — including one that
        // was never pinned at all.
        assert!(is_pinned(999_999), "overflow must answer conservatively");
        drop(pins);
        assert_eq!(
            OVERFLOW.load(Ordering::Acquire),
            before,
            "overflow must unwind as the lost pins are released"
        );
    }
}
