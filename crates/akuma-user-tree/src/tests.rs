//! Host model of a CoW fork: a fake page-table tree with per-frame reference counts, driven by the same
//! recursion the kernel runs (`taoxichuan/src/main.rs`: `clone_user_tree` / `free_subtree`) but taking
//! every decision from the crate's plans.

use super::*;
use std::collections::{BTreeMap, HashMap};

const L: PteLayout = X86_64;
const P: u64 = 1;
const W: u64 = 1 << 1;
const COW: u64 = 1 << 9;
const HUGE: u64 = 1 << 7;

#[derive(Default, Clone)]
struct Mem {
    /// table frame -> {index -> entry}
    tables: HashMap<u64, BTreeMap<usize, u64>>,
    /// leaf frame -> references
    refs: HashMap<u64, u32>,
    next: u64,
    allocs: usize,
    fail_at: Option<usize>,
}

impl Mem {
    fn new() -> Self {
        Self { next: 0x1000_0000, ..Self::default() }
    }

    fn bump(&mut self) -> Option<u64> {
        if self.fail_at == Some(self.allocs) {
            return None;
        }
        self.allocs += 1;
        let a = self.next;
        self.next += 0x1000;
        Some(a)
    }

    fn alloc_table(&mut self) -> Option<u64> {
        let a = self.bump()?;
        self.tables.insert(a, BTreeMap::new());
        Some(a)
    }

    fn alloc_leaf(&mut self) -> Option<u64> {
        let a = self.bump()?;
        self.refs.insert(a, 1);
        Some(a)
    }

    fn inc(&mut self, f: u64) {
        *self.refs.get_mut(&f).expect("share of a freed frame") += 1;
    }

    fn dec(&mut self, f: u64) {
        let r = self.refs.get_mut(&f).expect("free of a freed frame");
        *r -= 1;
        if *r == 0 {
            self.refs.remove(&f);
        }
    }
}

/// The kernel's `free_subtree`, via the plan.
fn free_table(m: &mut Mem, pa: u64, depth: u32) {
    let entries: std::vec::Vec<u64> = m.tables.get(&pa).expect("free of a freed table").values().copied().collect();
    for e in entries {
        match L.plan_free(depth, e) {
            None => {}
            Some(FreeStep::Leaf(f)) => m.dec(f),
            Some(FreeStep::Table(t)) => free_table(m, t, depth - 1),
        }
    }
    m.tables.remove(&pa);
}

/// The kernel's `clone_user_tree`, via the plan. Returns the new table or `None` (everything built is released).
fn clone_table(
    m: &mut Mem,
    src: u64,
    depth: u32,
    va_base: u64,
    shared: &dyn Fn(u64) -> bool,
    policy: SharedSpans,
) -> Option<u64> {
    let out = m.alloc_table()?;
    let shift = 12 + 9 * depth;
    let entries: std::vec::Vec<(usize, u64)> = m.tables[&src].iter().map(|(&i, &e)| (i, e)).collect();
    for (i, entry) in entries {
        let va = va_base | ((i as u64) << shift);
        match L.plan_clone(depth, entry, depth == 0 && shared(va), policy) {
            ClonePlan::Skip => {}
            ClonePlan::Refuse => {
                free_table(m, out, depth);
                return None;
            }
            ClonePlan::Descend { table } => {
                let Some(sub) = clone_table(m, table, depth - 1, va, shared, policy) else {
                    free_table(m, out, depth);
                    return None;
                };
                m.tables.get_mut(&out).unwrap().insert(i, L.relink(entry, sub));
            }
            ClonePlan::CopyLeaf { old } => {
                let Some(frame) = m.alloc_leaf() else {
                    free_table(m, out, depth);
                    return None;
                };
                assert!(m.refs.contains_key(&old));
                m.tables.get_mut(&out).unwrap().insert(i, L.relink(entry, frame));
            }
            ClonePlan::ShareLeaf { frame, parent_entry, child_entry, demoted } => {
                m.inc(frame);
                if demoted {
                    m.tables.get_mut(&src).unwrap().insert(i, parent_entry);
                }
                m.tables.get_mut(&out).unwrap().insert(i, child_entry);
            }
        }
    }
    Some(out)
}

/// A deterministic pseudo-random PDPT (depth 2) with `n_pd` PDs, each with a few PTs of mixed leaves.
fn build(m: &mut Mem, seed: u64) -> u64 {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut rnd = move || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (s >> 33) as usize
    };
    let pdpt = m.alloc_table().unwrap();
    for i3 in 0..(1 + rnd() % 3) {
        let pd = m.alloc_table().unwrap();
        m.tables.get_mut(&pdpt).unwrap().insert(i3, pd | P | W | 4);
        for i2 in 0..(1 + rnd() % 3) {
            let pt = m.alloc_table().unwrap();
            m.tables.get_mut(&pd).unwrap().insert(i2, pt | P | W | 4);
            for i1 in 0..(1 + rnd() % 9) {
                let f = m.alloc_leaf().unwrap();
                let flags = match rnd() % 3 {
                    0 => P | W | 4,
                    1 => P | 4,
                    _ => P | COW | 4,
                };
                m.tables.get_mut(&pt).unwrap().insert(i1 * 2, f | flags);
            }
        }
    }
    m.allocs = 0;
    pdpt
}

fn live_refs(m: &Mem) -> std::vec::Vec<(u64, u32)> {
    let mut v: std::vec::Vec<_> = m.refs.iter().map(|(&a, &r)| (a, r)).collect();
    v.sort_unstable();
    v
}

fn leaves(m: &Mem, pdpt: u64) -> std::vec::Vec<u64> {
    let mut out = std::vec::Vec::new();
    for e3 in m.tables[&pdpt].values() {
        for e2 in m.tables[&L.frame(*e3)].values() {
            out.extend(m.tables[&L.frame(*e2)].values().copied());
        }
    }
    out
}

fn no_span(_va: u64) -> bool {
    false
}

#[test]
fn plan_skip_and_refuse() {
    assert_eq!(L.plan_clone(0, 0, false, SharedSpans::Copy), ClonePlan::Skip);
    assert_eq!(L.plan_clone(2, 0, false, SharedSpans::Copy), ClonePlan::Skip);
    assert_eq!(L.plan_clone(1, 0x5000 | P | HUGE, false, SharedSpans::Copy), ClonePlan::Refuse);
    assert_eq!(L.plan_clone(2, 0x5000 | P | HUGE, false, SharedSpans::ShareFrames), ClonePlan::Refuse);
    // The huge bit on a LEAF (depth 0) is the PAT bit, not a page size: shared as usual.
    assert!(matches!(L.plan_clone(0, 0x5000 | P | HUGE, false, SharedSpans::Copy), ClonePlan::ShareLeaf { .. }));
    assert_eq!(L.plan_clone(2, 0x7000 | P | W, false, SharedSpans::Copy), ClonePlan::Descend { table: 0x7000 });
}

#[test]
fn plan_share_demotes_only_a_writable_leaf() {
    let w = 0x9000 | P | W | 4;
    assert_eq!(
        L.plan_clone(0, w, false, SharedSpans::Copy),
        ClonePlan::ShareLeaf { frame: 0x9000, parent_entry: 0x9000 | P | 4 | COW, child_entry: 0x9000 | P | 4 | COW, demoted: true }
    );
    let ro = 0x9000 | P | 4;
    assert_eq!(
        L.plan_clone(0, ro, false, SharedSpans::Copy),
        ClonePlan::ShareLeaf { frame: 0x9000, parent_entry: ro, child_entry: ro, demoted: false }
    );
    // Already a CoW share: left alone (no second marker, no flush).
    let already = 0x9000 | P | 4 | COW;
    assert!(matches!(L.plan_clone(0, already, false, SharedSpans::Copy), ClonePlan::ShareLeaf { demoted: false, .. }));
}

/// The divergence between the two kernels, pinned both ways.
#[test]
fn shared_span_policy_is_a_named_choice_with_a_test_per_value() {
    let e = 0xA000 | P | W | 4;
    // Copy (taoxichuan): the leaf is copied, never shared, and never demoted.
    assert_eq!(L.plan_clone(0, e, true, SharedSpans::Copy), ClonePlan::CopyLeaf { old: 0xA000 });
    // ShareFrames (amd64): the same leaf is an ordinary share.
    assert!(matches!(L.plan_clone(0, e, true, SharedSpans::ShareFrames), ClonePlan::ShareLeaf { demoted: true, .. }));
    // Outside a shared span the policy is irrelevant.
    assert_eq!(
        L.plan_clone(0, e, false, SharedSpans::Copy),
        L.plan_clone(0, e, false, SharedSpans::ShareFrames)
    );
}

#[test]
fn plan_free_visits_leaves_and_tables_and_skips_absent() {
    assert_eq!(L.plan_free(0, 0), None);
    assert_eq!(L.plan_free(0, 0x3000 | P | W), Some(FreeStep::Leaf(0x3000)));
    assert_eq!(L.plan_free(1, 0x3000 | P | W), Some(FreeStep::Table(0x3000)));
    assert_eq!(L.plan_free(2, 0x3000 | P), Some(FreeStep::Table(0x3000)));
}

#[test]
fn relink_keeps_flags_and_replaces_the_frame() {
    let e = 0xABC000 | P | W | 4 | COW | (1 << 63);
    assert_eq!(L.relink(e, 0x777000), 0x777000 | P | W | 4 | COW | (1 << 63));
}

#[test]
fn clone_is_zero_sum_and_demotes_exactly_the_writable_leaves() {
    for seed in 0..60 {
        for policy in [SharedSpans::Copy, SharedSpans::ShareFrames] {
            let mut m = Mem::new();
            let parent = build(&mut m, seed);
            let before = live_refs(&m);
            let before_w: usize = leaves(&m, parent).iter().filter(|e| *e & W != 0).count();
            let n_leaves = leaves(&m, parent).len();
            let child = clone_table(&mut m, parent, 2, 0, &no_span, policy).expect("clone");
            // No new leaf frame: every leaf is now shared by exactly two tables.
            let after = live_refs(&m);
            assert_eq!(after.len(), before.len(), "seed {seed}: a clone with no shared span creates no leaf frame");
            for ((a0, r0), (a1, r1)) in before.iter().zip(&after) {
                assert_eq!(a0, a1);
                assert_eq!(*r1, r0 + 1, "seed {seed}: each shared frame gains exactly one reference");
            }
            // No writable leaf survives in either tree; the marker is on everything that was writable.
            let (pl, cl) = (leaves(&m, parent), leaves(&m, child));
            assert_eq!(pl.len(), n_leaves);
            assert_eq!(pl, cl, "seed {seed}: parent and child leaf entries are identical after the share");
            assert!(pl.iter().all(|e| e & W == 0), "seed {seed}: no writable leaf remains");
            let marked_now = pl.iter().filter(|e| *e & COW != 0).count();
            assert!(marked_now >= before_w, "seed {seed}: every formerly writable leaf carries the marker");
        }
    }
}

/// The host twin of the leak gate: clone, release both, and nothing is left.
#[test]
fn freeing_clone_and_parent_returns_every_reference_and_table() {
    for seed in 0..60 {
        for policy in [SharedSpans::Copy, SharedSpans::ShareFrames] {
            let mut m = Mem::new();
            let parent = build(&mut m, seed);
            let child = clone_table(&mut m, parent, 2, 0, &no_span, policy).unwrap();
            free_table(&mut m, child, 2);
            // After the child alone is gone, the parent holds exactly one reference per leaf again.
            assert!(m.refs.values().all(|&r| r == 1), "seed {seed}: child release drops exactly its references");
            free_table(&mut m, parent, 2);
            assert!(m.refs.is_empty(), "seed {seed}: no leaf frame outlives both trees");
            assert!(m.tables.is_empty(), "seed {seed}: no table outlives both trees");
        }
    }
}

/// Out of frames at EVERY possible allocation point: the partial clone is released and the books balance.
#[test]
fn a_failed_clone_leaves_every_reference_as_it_was() {
    for seed in 0..20 {
        // A shared span in the middle so the Copy policy allocates leaf frames too.
        let span = |va: u64| va & 0x1000 != 0;
        for policy in [SharedSpans::Copy, SharedSpans::ShareFrames] {
            let mut probe = Mem::new();
            let parent = build(&mut probe, seed);
            let _ = clone_table(&mut probe, parent, 2, 0, &span, policy).unwrap();
            let total = probe.allocs;
            for k in 0..total {
                let mut m = Mem::new();
                let parent = build(&mut m, seed);
                let tables_before = m.tables.len();
                let refs_before = live_refs(&m);
                m.fail_at = Some(k);
                assert!(clone_table(&mut m, parent, 2, 0, &span, policy).is_none(), "seed {seed} k {k}");
                m.fail_at = None;
                assert_eq!(m.tables.len(), tables_before, "seed {seed} k {k} {policy:?}: no table leaked");
                assert_eq!(live_refs(&m), refs_before, "seed {seed} k {k} {policy:?}: references exactly as before");
                // And the parent is still whole: freeing it empties everything.
                free_table(&mut m, parent, 2);
                assert!(m.refs.is_empty() && m.tables.is_empty(), "seed {seed} k {k}: parent intact after a failed fork");
            }
        }
    }
}

#[test]
fn a_huge_page_refuses_the_fork_and_releases_what_was_built() {
    let mut m = Mem::new();
    let parent = build(&mut m, 7);
    // Turn one PD entry into a 2 MiB leaf.
    let pd = L.frame(*m.tables[&parent].values().next().unwrap());
    let key = *m.tables[&pd].keys().next().unwrap();
    m.tables.get_mut(&pd).unwrap().insert(key, 0x4000_0000 | P | W | HUGE | 4);
    let tables = m.tables.len();
    assert!(clone_table(&mut m, parent, 2, 0, &no_span, SharedSpans::Copy).is_none());
    assert_eq!(m.tables.len(), tables, "the refused fork leaked no table");
}

#[test]
fn shared_span_copy_creates_a_distinct_frame_and_share_frames_does_not() {
    let all = |_va: u64| true;
    let mut a = Mem::new();
    let pa = build(&mut a, 3);
    let n = leaves(&a, pa).len();
    let parent_before = leaves(&a, pa);
    let copied_child = clone_table(&mut a, pa, 2, 0, &all, SharedSpans::Copy).unwrap();
    assert_eq!(a.refs.len(), 2 * n, "Copy: every leaf got its own new frame");
    assert!(a.refs.values().all(|&r| r == 1), "Copy: nothing is shared");
    assert_eq!(leaves(&a, pa), parent_before, "Copy: the parent's entries are untouched (not demoted)");
    free_table(&mut a, copied_child, 2);
    free_table(&mut a, pa, 2);
    assert!(a.refs.is_empty() && a.tables.is_empty());

    let mut b = Mem::new();
    let pb = build(&mut b, 3);
    let shared_child = clone_table(&mut b, pb, 2, 0, &all, SharedSpans::ShareFrames).unwrap();
    assert_eq!(b.refs.len(), n, "ShareFrames: no new leaf frame");
    assert!(b.refs.values().all(|&r| r == 2), "ShareFrames: every leaf is held twice");
    free_table(&mut b, shared_child, 2);
    free_table(&mut b, pb, 2);
    assert!(b.refs.is_empty() && b.tables.is_empty());
}
