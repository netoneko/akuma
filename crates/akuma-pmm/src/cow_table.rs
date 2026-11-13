//! The CoW share-count table: `physical address -> number of address spaces holding the frame`.
//!
//! It used to be `BTreeMap<usize, u16>`: one heap node per shared frame, so a fork of a 100k-page process made
//! 100k allocations *while the lock was held* and a build's worth of forks churned the heap continuously. This is
//! a two-level table instead: a fixed array of chunk pointers, each chunk 4096 `u16` counters covering 16 MiB of
//! physical address space, allocated on the first share in its range and freed when its last count drops. A share
//! costs no allocation unless it opens a new chunk (8 KiB per 16 MiB at most); `get`/`inc`/`dec` are O(1).
//!
//! Addresses at or above [`FLAT_LIMIT`] (64 GiB) fall back to a `BTreeMap`, so the table is total over `usize`
//! and no caller has to know the cutoff exists.
//!
//! # Contract (unchanged from the map it replaces)
//! * absent == 0 == "not shared": a single owner, safe to free;
//! * the first `inc` of an absent frame stores **2** (parent + child), later ones add 1 (saturating);
//! * `dec` of a present frame subtracts 1 and removes the entry at 0; `dec` of an absent frame reports
//!   "last owner" without changing anything.
//!
//! The lock stays outside (`COW_REFCOUNTS` in `lib.rs`): this type is plain data and takes nothing.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

const PAGE_SHIFT: usize = 12;
const CHUNK_FRAMES: usize = 4096;
const CHUNK_SHIFT: usize = PAGE_SHIFT + 12; // 16 MiB
const N_CHUNKS: usize = 4096;
/// First physical address the flat table does not cover (64 GiB); the overflow map takes over from here.
pub const FLAT_LIMIT: usize = N_CHUNKS << CHUNK_SHIFT;

struct Chunk {
    counts: [u16; CHUNK_FRAMES],
    /// Non-zero counters in `counts`; the chunk is freed when this reaches 0.
    live: u16,
}

pub struct CowTable {
    chunks: [Option<Box<Chunk>>; N_CHUNKS],
    overflow: BTreeMap<usize, u16>,
    len: usize,
}

impl CowTable {
    // Evaluated at compile time to initialise a `static` (the 32 KiB array never exists on a stack).
    #[allow(clippy::large_stack_arrays)]
    pub const fn new() -> Self {
        Self { chunks: [const { None }; N_CHUNKS], overflow: BTreeMap::new(), len: 0 }
    }

    /// Entries (frames with a non-zero count).
    pub fn len(&self) -> usize {
        self.len
    }

    /// The count for `pa`, 0 if absent.
    pub fn get(&self, pa: usize) -> u16 {
        if pa >= FLAT_LIMIT {
            return self.overflow.get(&pa).copied().unwrap_or(0);
        }
        match &self.chunks[pa >> CHUNK_SHIFT] {
            Some(c) => c.counts[(pa >> PAGE_SHIFT) & (CHUNK_FRAMES - 1)],
            None => 0,
        }
    }

    /// Set the count for `pa`; `0` removes the entry. Keeps `len` and the chunk live-counts exact.
    fn set(&mut self, pa: usize, v: u16) {
        if pa >= FLAT_LIMIT {
            let had = self.overflow.contains_key(&pa);
            if v == 0 {
                if had {
                    self.overflow.remove(&pa);
                    self.len -= 1;
                }
            } else {
                self.overflow.insert(pa, v);
                if !had {
                    self.len += 1;
                }
            }
            return;
        }
        let ci = pa >> CHUNK_SHIFT;
        let fi = (pa >> PAGE_SHIFT) & (CHUNK_FRAMES - 1);
        if v == 0 {
            let Some(chunk) = self.chunks[ci].as_mut() else { return };
            if chunk.counts[fi] == 0 {
                return;
            }
            chunk.counts[fi] = 0;
            chunk.live -= 1;
            self.len -= 1;
            if chunk.live == 0 {
                self.chunks[ci] = None;
            }
            return;
        }
        let chunk = self.chunks[ci].get_or_insert_with(|| Box::new(Chunk { counts: [0; CHUNK_FRAMES], live: 0 }));
        if chunk.counts[fi] == 0 {
            chunk.live += 1;
            self.len += 1;
        }
        chunk.counts[fi] = v;
    }

    /// Take a reference. Returns `(created, before, after)`: `created` is true when the entry did not exist
    /// (the resurrection detector in `cow_ref_inc` needs the distinction from an existing count of 1).
    pub fn inc(&mut self, pa: usize) -> (bool, u16, u16) {
        match self.get(pa) {
            0 => {
                self.set(pa, 2);
                (true, 0, 2)
            }
            before => {
                let after = before.saturating_add(1);
                self.set(pa, after);
                (false, before, after)
            }
        }
    }

    /// Drop a reference. Returns `(before, after, last)`: `last` means the caller should free the frame
    /// (the count reached 0, or the frame was never tracked: a single owner).
    pub fn dec(&mut self, pa: usize) -> (u16, u16, bool) {
        match self.get(pa) {
            0 => (0, 0, true),
            before => {
                let after = before.saturating_sub(1);
                self.set(pa, after);
                (before, after, after == 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// The map this type replaced, with its exact `inc`/`dec` bodies, as the oracle.
    #[derive(Default)]
    struct Old(BTreeMap<usize, u16>);
    impl Old {
        fn inc(&mut self, pa: usize) -> (bool, u16, u16) {
            match self.0.get_mut(&pa) {
                Some(e) => {
                    let b = *e;
                    *e = e.saturating_add(1);
                    (false, b, *e)
                }
                None => {
                    self.0.insert(pa, 2);
                    (true, 0, 2)
                }
            }
        }
        fn dec(&mut self, pa: usize) -> (u16, u16, bool) {
            match self.0.get_mut(&pa) {
                Some(c) => {
                    let b = *c;
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        self.0.remove(&pa);
                        (b, 0, true)
                    } else {
                        (b, *c, false)
                    }
                }
                None => (0, 0, true),
            }
        }
    }

    fn lcg(s: &mut u64) -> u64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *s >> 33
    }

    /// Random operation streams over a spread of addresses that cross chunk boundaries and the 64 GiB cutoff,
    /// compared with the old map after every step.
    #[test]
    fn behaves_exactly_like_the_btreemap_it_replaced() {
        let bases = [
            0x4000_0000usize,
            0xCAFE_0000,
            (1 << CHUNK_SHIFT) - 0x2000, // straddles a chunk edge
            FLAT_LIMIT - 0x3000,         // last flat frames
            FLAT_LIMIT,                  // first overflow frame
            FLAT_LIMIT + 0x1_0000_0000,
        ];
        for seed in 0..40u64 {
            let mut s = seed + 1;
            let (mut new, mut old) = (CowTable::new(), Old::default());
            for _ in 0..4000 {
                let pa = (bases[lcg(&mut s) as usize % bases.len()] + (lcg(&mut s) as usize % 6) * 0x1000) & !0xFFF;
                if lcg(&mut s) % 5 < 3 {
                    assert_eq!(new.inc(pa), old.inc(pa), "seed {seed} inc {pa:#x}");
                } else {
                    assert_eq!(new.dec(pa), old.dec(pa), "seed {seed} dec {pa:#x}");
                }
                assert_eq!(new.get(pa), old.0.get(&pa).copied().unwrap_or(0));
                assert_eq!(new.len(), old.0.len(), "seed {seed}: len after {pa:#x}");
            }
            for &b in &bases {
                for k in 0..6 {
                    let pa = b + k * 0x1000;
                    assert_eq!(new.get(pa), old.0.get(&pa).copied().unwrap_or(0));
                }
            }
        }
    }

    #[test]
    fn first_inc_is_two_and_the_chunk_is_freed_with_its_last_count() {
        let mut t = CowTable::new();
        let pa = 0x8000_0000usize;
        assert_eq!(t.inc(pa), (true, 0, 2));
        assert_eq!(t.inc(pa), (false, 2, 3));
        assert!(t.chunks[pa >> CHUNK_SHIFT].is_some());
        assert_eq!(t.dec(pa), (3, 2, false));
        assert_eq!(t.dec(pa), (2, 1, false));
        assert_eq!(t.dec(pa), (1, 0, true));
        assert!(t.chunks[pa >> CHUNK_SHIFT].is_none(), "no chunk outlives its last entry");
        assert_eq!(t.len(), 0);
        assert_eq!(t.dec(pa), (0, 0, true), "an untracked frame is a single owner");
    }

    #[test]
    fn a_hundred_thousand_shares_open_a_handful_of_chunks_not_a_hundred_thousand_nodes() {
        let mut t = CowTable::new();
        for i in 0..100_000usize {
            t.inc(0x4000_0000 + i * 0x1000);
        }
        assert_eq!(t.len(), 100_000);
        let chunks = t.chunks.iter().filter(|c| c.is_some()).count();
        assert_eq!(chunks, 100_000usize.div_ceil(CHUNK_FRAMES), "one 8 KiB chunk per 4096 frames");
        for i in 0..100_000usize {
            assert_eq!(t.dec(0x4000_0000 + i * 0x1000), (2, 1, false));
            assert_eq!(t.dec(0x4000_0000 + i * 0x1000), (1, 0, true));
        }
        assert_eq!(t.len(), 0);
        assert!(t.chunks.iter().all(Option::is_none), "everything freed");
    }

    #[test]
    fn counts_saturate_at_u16_max() {
        let mut t = CowTable::new();
        let pa = 0x1000_0000;
        t.inc(pa);
        for _ in 0..70_000 {
            t.inc(pa);
        }
        assert_eq!(t.get(pa), u16::MAX);
    }
}
