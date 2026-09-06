//! Host tests for the frame ledger.
//!
//! Everything here used to require a booted kernel to exercise, and the bugs it
//! pins were all found that way — by a self-host build dying hours in, or by a
//! frame handed back to the PMM while it was still mapped.
//!
//! The two counts and the rule between them are the whole subject; see the
//! module header. `adopt_user_frame`'s four-way outcome is the part where a
//! wrong answer leaks a frame forever (too few releases) or frees a live one
//! (too many), and neither shows up where it happened.

extern crate std;

use super::*;

fn frame(addr: usize) -> PhysFrame {
    PhysFrame::new(addr)
}

const A: usize = 0x1000;
const B: usize = 0x2000;

#[test]
fn a_new_ledger_is_empty() {
    let l = FrameLedger::new(false);
    assert_eq!(l.user_frame_count(), 0);
    assert_eq!(l.user_frame_total_refs(), 0);
    assert_eq!(l.page_table_frame_count(), 0);
    assert!(!l.is_shared());
    assert!(!l.tracks_user_frame(A));
}

/// One frame at two VAs is **one** entry with a count of two. The distinction
/// is the whole reason there are two counters: teardown frees per entry, and
/// treating the count as the entry count frees the frame twice.
#[test]
fn one_frame_at_two_vas_is_one_entry_with_two_refs() {
    let l = FrameLedger::new(false);
    l.track_user_frame(frame(A));
    l.track_user_frame(frame(A));
    assert_eq!(l.user_frame_count(), 1, "one distinct frame");
    assert_eq!(l.user_frame_total_refs(), 2, "two VAs map it");
}

/// `remove_user_frame` returns the free obligation, and only on the transition
/// to zero. A caller that frees on every `true` would otherwise free a frame
/// still mapped at another VA.
#[test]
fn only_the_last_removal_transfers_the_free_obligation() {
    let l = FrameLedger::new(false);
    l.track_user_frame(frame(A));
    l.track_user_frame(frame(A));
    assert!(!l.remove_user_frame(frame(A)), "still mapped elsewhere");
    assert!(l.remove_user_frame(frame(A)), "last VA — caller owns the free");
    assert_eq!(l.user_frame_count(), 0);
    assert!(!l.tracks_user_frame(A));
}

/// Removing something this ledger never tracked is `false`, not a panic and not
/// a free: it is another address space's frame, and freeing it would be
/// somebody else's use-after-free.
#[test]
fn removing_an_untracked_frame_claims_nothing() {
    let l = FrameLedger::new(false);
    assert!(!l.remove_user_frame(frame(A)));
    l.track_user_frame(frame(A));
    assert!(!l.remove_user_frame(frame(B)), "B was never here");
    assert_eq!(l.user_frame_count(), 1, "A is untouched");
}

/// A double free through the ledger cannot resurrect an entry.
#[test]
fn removing_past_zero_stays_at_zero() {
    let l = FrameLedger::new(false);
    l.track_user_frame(frame(A));
    assert!(l.remove_user_frame(frame(A)));
    assert!(!l.remove_user_frame(frame(A)), "second free claims nothing");
    assert_eq!(l.user_frame_count(), 0);
}

// ---------------------------------------------------------------------------
// adopt_user_frame — the four-way rule
// ---------------------------------------------------------------------------

/// Caller holds a reference and this is the first VA here: the caller's
/// reference *becomes* this address space's one reference. Nothing surplus.
#[test]
fn adopt_first_va_with_a_held_ref_consumes_it() {
    let l = FrameLedger::new(false);
    assert!(!l.adopt_user_frame(frame(A), true), "not surplus");
    assert_eq!(l.user_frame_total_refs(), 1);
}

/// Caller holds a reference but this address space already had the frame: the
/// address space contributes exactly one global reference however many VAs map
/// it, so the caller's is **surplus** and must be handed back. Returning
/// `false` here is the leak-until-reboot bug.
#[test]
fn adopt_second_va_with_a_held_ref_reports_it_surplus() {
    let l = FrameLedger::new(false);
    assert!(!l.adopt_user_frame(frame(A), true));
    assert!(l.adopt_user_frame(frame(A), true), "surplus — caller must release");
    assert_eq!(l.user_frame_count(), 1);
    assert_eq!(l.user_frame_total_refs(), 2);
}

/// Caller holds no reference and this is a second VA: an earlier VA already
/// took the one global reference, so nothing is owed either way.
#[test]
fn adopt_second_va_without_a_held_ref_is_a_no_op_globally() {
    let l = FrameLedger::new(false);
    l.track_user_frame(frame(A));
    assert!(!l.adopt_user_frame(frame(A), false));
    assert_eq!(l.user_frame_total_refs(), 2);
}

/// However many VAs adopt a frame, the ledger reports exactly one entry — which
/// is the number of global references this address space is responsible for.
#[test]
fn many_vas_still_owe_exactly_one_global_reference() {
    let l = FrameLedger::new(false);
    let mut surplus = 0;
    for _ in 0..8 {
        if l.adopt_user_frame(frame(A), true) {
            surplus += 1;
        }
    }
    assert_eq!(l.user_frame_count(), 1, "one frame, one global reference owed");
    assert_eq!(l.user_frame_total_refs(), 8);
    assert_eq!(surplus, 7, "every adoption after the first hands its ref back");
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

/// Teardown frees each **distinct** frame once, ignoring the counts. Draining
/// and freeing per-count is a double free; this pins the shape the caller must
/// use.
#[test]
fn taking_the_map_yields_distinct_frames_and_empties_the_ledger() {
    let l = FrameLedger::new(false);
    l.track_user_frame(frame(A));
    l.track_user_frame(frame(A));
    l.track_user_frame(frame(B));

    let taken = l.take_user_frames();
    assert_eq!(taken.len(), 2, "two distinct frames");
    assert_eq!(taken[&A], 2, "the count is preserved for accounting");
    assert_eq!(taken[&B], 1);
    let freed: std::vec::Vec<usize> = taken.keys().copied().collect();
    assert_eq!(freed, std::vec![A, B], "one free per key, not per count");

    assert_eq!(l.user_frame_count(), 0, "ledger is empty afterwards");
    assert!(!l.tracks_user_frame(A));
}

#[test]
fn taking_page_table_frames_empties_that_list_too() {
    let l = FrameLedger::new(false);
    l.track_page_table_frame(frame(A));
    l.track_page_table_frame(frame(B));
    assert_eq!(l.page_table_frame_count(), 2);
    let taken = l.take_page_table_frames();
    assert_eq!(taken.len(), 2);
    assert_eq!(l.page_table_frame_count(), 0);
}

/// Page-table frames are a list, not a set: the walker pushes each frame it
/// allocates and they are distinct by construction. Pinned so a future change
/// to a `BTreeSet` is a deliberate one.
#[test]
fn page_table_frames_are_a_list_and_keep_duplicates() {
    let l = FrameLedger::new(false);
    l.track_page_table_frame(frame(A));
    l.track_page_table_frame(frame(A));
    assert_eq!(l.page_table_frame_count(), 2);
}

// ---------------------------------------------------------------------------
// resident_pages / shared views
// ---------------------------------------------------------------------------

/// `+ 1` for the top-level table, which the ledger does not hold but the
/// address space still owes.
#[test]
fn resident_pages_counts_user_plus_tables_plus_the_root() {
    let l = FrameLedger::new(false);
    assert_eq!(l.resident_pages(), 1, "empty: just the root table");
    l.track_user_frame(frame(A));
    l.track_user_frame(frame(A)); // second VA, same frame — not a second page
    l.track_page_table_frame(frame(B));
    assert_eq!(l.resident_pages(), 1 + 1 + 1);
}

/// A shared view (a `vfork` child borrowing its parent's tables) owns nothing
/// and must report zero — reclaim sizing a backlog from it would otherwise
/// double-count the parent's memory, and a teardown acting on it would free
/// pages still in use.
#[test]
fn a_shared_view_owns_nothing() {
    let l = FrameLedger::new(true);
    assert!(l.is_shared());
    l.track_user_frame(frame(A));
    l.track_page_table_frame(frame(B));
    assert_eq!(l.resident_pages(), 0, "shared views own no pages");
    // The maps still answer honestly; only ownership is disclaimed.
    assert_eq!(l.user_frame_count(), 1);
}
