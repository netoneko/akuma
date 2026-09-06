//! Host tests for the CoW decision.
//!
//! Each of these corresponds to an incident. The decision is four lines of
//! code; the reason it is a crate is that getting any of the four wrong is
//! invisible where it happens.

use super::*;

fn f(pte_writable: bool, marked: bool, refs: u16) -> CowFault {
    CowFault { pte_writable, marked, refs }
}

/// A page the table already permits writing is not a fault to judge. Killing
/// the process here is the `cowstale` spurious-SIGSEGV class
/// (`[WPF] … ap_rw=true`).
#[test]
fn an_already_writable_page_retries_whatever_else_is_true() {
    for marked in [false, true] {
        for refs in [0u16, 1, 2, 9] {
            assert_eq!(
                f(true, marked, refs).decide(),
                CowAction::Retry,
                "writable PTE must retry (marked={marked} refs={refs})"
            );
        }
    }
}

/// **The grant-vs-deny rule.** An unmarked read-only page is read-only on
/// purpose. Promoting it because its frame happens to be shared is how
/// `mprotect(PROT_READ)` silently stops working across a `fork`.
#[test]
fn an_unmarked_page_faults_however_shared_its_frame_is() {
    for refs in [0u16, 1, 2, 1000] {
        assert_eq!(
            f(false, false, refs).decide(),
            CowAction::Fault,
            "unmarked pages are not CoW (refs={refs})"
        );
    }
}

/// `0` is the PMM's "untracked → single owner"; `1` is tracked with one holder.
/// Both mean nobody else can see the page.
#[test]
fn sole_ownership_takes_the_page_in_place() {
    assert_eq!(f(false, true, 0).decide(), CowAction::TakeInPlace, "untracked");
    assert_eq!(f(false, true, 1).decide(), CowAction::TakeInPlace, "one holder");
}

#[test]
fn a_genuinely_shared_page_is_copied() {
    assert_eq!(f(false, true, 2).decide(), CowAction::Copy);
    assert_eq!(f(false, true, u16::MAX).decide(), CowAction::Copy);
}

/// The boundary is where a wrong `<` vs `<=` silently shares a page between two
/// processes, so pin both sides of it.
#[test]
fn the_ownership_boundary_is_between_one_and_two() {
    assert_eq!(f(false, true, 1).decide(), CowAction::TakeInPlace);
    assert_eq!(f(false, true, 2).decide(), CowAction::Copy);
}

/// Only `Copy` allocates, which is what lets a handler put its
/// out-of-memory arm in exactly one place.
#[test]
fn copy_is_the_only_action_that_needs_a_frame() {
    for (a, want) in [
        (CowAction::Retry, false),
        (CowAction::Fault, false),
        (CowAction::TakeInPlace, false),
        (CowAction::Copy, true),
    ] {
        assert_eq!(a.needs_frame(), want, "{a:?}");
    }
}

/// Everything but `Fault` leaves the faulting instruction safe to re-execute.
#[test]
fn only_fault_does_not_resolve_the_write() {
    assert!(CowAction::Retry.resolves());
    assert!(CowAction::TakeInPlace.resolves());
    assert!(CowAction::Copy.resolves());
    assert!(!CowAction::Fault.resolves());
}

/// The ordering of the tests is the substance — `pte_writable` outranks
/// everything, and `marked` outranks `refs`. Pin both precedences directly,
/// because a reordering still compiles and still passes every single-condition
/// test above.
#[test]
fn precedence_is_writable_then_marked_then_refs() {
    // writable beats "not marked" (which would otherwise Fault)
    assert_eq!(f(true, false, 0).decide(), CowAction::Retry);
    // writable beats "shared" (which would otherwise Copy)
    assert_eq!(f(true, true, 5).decide(), CowAction::Retry);
    // not-marked beats "shared" (which would otherwise Copy)
    assert_eq!(f(false, false, 5).decide(), CowAction::Fault);
}
