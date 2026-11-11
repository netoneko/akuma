extern crate std;
use super::*;

#[test]
fn check_order_is_size_then_range_then_unchangeable() {
    assert_eq!(SigTable::check(2, false, 8), Ok(()));
    assert_eq!(SigTable::check(2, false, 4), Err(SigError::Inval));
    assert_eq!(SigTable::check(0, false, 8), Err(SigError::Inval));
    assert_eq!(SigTable::check(65, false, 8), Err(SigError::Inval));
    assert_eq!(SigTable::check(64, true, 8), Ok(()));
    // SIGKILL/SIGSTOP: readable, not changeable.
    for s in [SIGKILL, SIGSTOP] {
        assert_eq!(SigTable::check(s, false, 8), Ok(()));
        assert_eq!(SigTable::check(s, true, 8), Err(SigError::Inval));
    }
}

/// Exhaustive over every signal number around the range and both `has_act`s.
#[test]
fn check_is_exhaustively_the_old_inline_rule() {
    for sig in 0..=70u64 {
        for has_act in [false, true] {
            for size in [0u64, 7, 8, 16] {
                let old = if size != 8 || sig == 0 || sig > 64 {
                    Err(SigError::Inval)
                } else if has_act && (sig == 9 || sig == 19) {
                    Err(SigError::Inval)
                } else {
                    Ok(())
                };
                assert_eq!(SigTable::check(sig, has_act, size), old, "sig {sig} act {has_act} size {size}");
            }
        }
    }
}

#[test]
fn replace_reads_back_the_old_action_the_musl_signal_regression() {
    let mut t = SigTable::new();
    // First install: the OLD action is the default, never garbage.
    assert_eq!(t.replace(13, [SIG_IGN, 0, 0, 0]), [0; 4]);
    // std's child-side signal(SIGPIPE, SIG_DFL) reads back SIG_IGN.
    assert_eq!(t.replace(13, [SIG_DFL, 0, 0, 0]), [SIG_IGN, 0, 0, 0]);
    assert_eq!(t.get(13), [0; 4]);
}

#[test]
fn out_of_range_never_panics_and_never_stores() {
    let mut t = SigTable::new();
    for s in [0u64, 65, 66, u64::MAX] {
        assert_eq!(t.replace(s, [5, 5, 5, 5]), [0; 4]);
        assert_eq!(t.get(s), [0; 4]);
    }
    assert_eq!(t, SigTable::new());
}

#[test]
fn exec_resets_caught_keeps_ignored() {
    let mut t = SigTable::new();
    t.replace(2, [0xdead_beef, 4, 0x1000, 7]); // caught
    t.replace(13, [SIG_IGN, 0, 0, 0]); // ignored
    t.replace(15, [SIG_DFL, 0x1234, 0, 0]); // default action with flags
    t.reset_for_exec();
    assert_eq!(t.get(2), [0; 4]);
    assert_eq!(t.get(13), [SIG_IGN, 0, 0, 0]);
    assert_eq!(t.get(15), [0; 4]);
}

#[test]
fn fork_inherits_by_value() {
    let mut parent = SigTable::new();
    parent.replace(13, [SIG_IGN, 0, 0, 0]);
    let mut child = parent;
    child.replace(13, [SIG_DFL, 0, 0, 0]);
    assert_eq!(parent.get(13), [SIG_IGN, 0, 0, 0]);
}

#[test]
fn wire_round_trip_and_layout() {
    let a: SigAction = [0x1122_3344_5566_7788, 0x0400_0000, 0xaabb_ccdd_eeff_0011, u64::MAX];
    let w = to_wire(&a);
    assert_eq!(&w[..8], &0x1122_3344_5566_7788u64.to_le_bytes());
    assert_eq!(&w[16..24], &0xaabb_ccdd_eeff_0011u64.to_le_bytes());
    assert_eq!(from_wire(&w), a);
}

#[test]
fn procmask_semantics() {
    let size = SIGSET_SIZE;
    assert_eq!(procmask(0, 0b0101, Some(0b0010), size), Ok(0b0111)); // BLOCK
    assert_eq!(procmask(1, 0b0111, Some(0b0010), size), Ok(0b0101)); // UNBLOCK
    assert_eq!(procmask(2, 0b0111, Some(0b1000), size), Ok(0b1000)); // SETMASK
    // No set: pure read, `how` is not even looked at.
    assert_eq!(procmask(99, 0b11, None, size), Ok(0b11));
    // Bad how with a set; bad size always.
    assert_eq!(procmask(3, 0, Some(1), size), Err(SigError::Inval));
    assert_eq!(procmask(0, 0, None, 4), Err(SigError::Inval));
}

#[test]
fn sigkill_and_sigstop_can_never_be_blocked() {
    for how in 0..=2 {
        let got = procmask(how, 0, Some(u64::MAX), SIGSET_SIZE).unwrap();
        assert_eq!(got & UNBLOCKABLE, 0, "how {how}");
    }
    // ...and a mask that somehow holds them is not made worse by UNBLOCK.
    assert_eq!(procmask(1, UNBLOCKABLE | 1, Some(1), SIGSET_SIZE), Ok(UNBLOCKABLE));
    assert_eq!(UNBLOCKABLE, (1 << 8) | (1 << 18));
}

/// SIG_BLOCK then SIG_UNBLOCK of the same set restores the mask for any
/// unblockable-free set (what musl's `pthread_sigmask` pair relies on).
#[test]
fn block_then_unblock_restores() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..1000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let cur = x & !UNBLOCKABLE;
        let set = x.rotate_left(17) & !UNBLOCKABLE & !cur; // disjoint from cur
        let blocked = procmask(0, cur, Some(set), SIGSET_SIZE).unwrap();
        assert_eq!(procmask(1, blocked, Some(set), SIGSET_SIZE), Ok(cur));
    }
}

/// `rt_sigprocmask` before the AArch64/amd64 glue adopted this crate: the formula it inlined, kept here
/// verbatim as the oracle. Exhaustive over `how` (including invalid values) and over masks that exercise
/// the unblockable bits (8 = SIGKILL, 18 = SIGSTOP) alone and with company.
#[test]
fn procmask_matches_the_glue_formula_it_replaced() {
    fn old(how: u64, cur: u64, new_mask: u64) -> Result<u64, ()> {
        let unblockable = (1u64 << 8) | (1u64 << 18);
        Ok(match how {
            0 => cur | (new_mask & !unblockable),
            1 => cur & !new_mask,
            2 => new_mask & !unblockable,
            _ => return Err(()),
        })
    }
    let masks = [
        0u64,
        1,
        1 << 8,
        1 << 18,
        (1 << 8) | (1 << 18),
        0xFFFF_FFFF_FFFF_FFFF,
        0x0000_0000_0004_0100,
        0x8000_0000_0000_0001,
        0x5555_5555_5555_5555,
        0xAAAA_AAAA_AAAA_AAAA,
    ];
    for how in 0..6u64 {
        for &cur in &masks {
            for &new in &masks {
                assert_eq!(
                    procmask(how, cur, Some(new), 8).map_err(|_| ()),
                    old(how, cur, new),
                    "how {how} cur {cur:#x} new {new:#x}"
                );
            }
        }
    }
}

#[test]
fn altstack_flag_rules() {
    // Disable wins over any size, autodisarm may ride along.
    assert_eq!(altstack_change(SS_DISABLE, 0), Ok(AltStackChange::Disable));
    assert_eq!(altstack_change(SS_DISABLE | i32::MIN, 0), Ok(AltStackChange::Disable));
    // SS_ONSTACK (1) and unknown bits are refused before the size is looked at.
    assert_eq!(altstack_change(1, 1 << 20), Err(AltStackError::Inval));
    assert_eq!(altstack_change(4, 1 << 20), Err(AltStackError::Inval));
    // Enabled: the MINSIGSTKSZ boundary is inclusive.
    assert_eq!(altstack_change(0, MINSIGSTKSZ - 1), Err(AltStackError::NoMem));
    assert_eq!(altstack_change(0, MINSIGSTKSZ), Ok(AltStackChange::Install));
    assert_eq!(altstack_change(i32::MIN, MINSIGSTKSZ), Ok(AltStackChange::Install));
}

// ── tkill_action ─────────────────────────────────────────────────────────────

const DISPOSITIONS: [Disposition; 3] = [Disposition::Ignore, Disposition::Default, Disposition::Handler];

/// Every target `tkill_action` can be asked about: no process, or one of the
/// three dispositions.
fn targets() -> impl Iterator<Item = Option<Disposition>> {
    core::iter::once(None).chain(DISPOSITIONS.into_iter().map(Some))
}

#[test]
fn fatal_by_default_is_linuxs_table() {
    // Ignored by default, or stop/continue (a no-op here): everything else kills.
    let not_fatal = [17, 18, 19, 20, 21, 22, 23, 28];
    for sig in 1..=64 {
        assert_eq!(fatal_by_default(sig), !not_fatal.contains(&sig), "sig {sig}");
    }
    for sig in [0, 65, 100, u64::MAX] {
        assert!(!fatal_by_default(sig), "sig {sig} is not a signal");
    }
    // The ones the pre-2026-11-04 table left out are fatal now.
    for sig in [10, 12, 16, 29, 30, 32, 34, 64] {
        assert!(fatal_by_default(sig), "sig {sig}");
    }
}

#[test]
fn a_thread_with_no_process_drops_everything() {
    for sig in 1..=64 {
        for mask in [0, u64::MAX, bit(sig)] {
            assert_eq!(tkill_action(sig, None, mask), TkillAction::Drop, "sig {sig} mask {mask:#x}");
        }
    }
}

#[test]
fn each_branch() {
    use Disposition::{Default, Handler, Ignore};
    use TkillAction::{Drop, Pend, PendInterrupt};
    const TERM: u64 = 15;
    const CHLD: u64 = 17;
    // SIGKILL: no mask, no handler, no ignore.
    for d in DISPOSITIONS {
        assert_eq!(tkill_action(SIGKILL, Some(d), u64::MAX), PendInterrupt);
        assert_eq!(tkill_action(SIGKILL, Some(d), 0), PendInterrupt);
    }
    assert_eq!(tkill_action(TERM, Some(Ignore), 0), Drop);
    assert_eq!(tkill_action(TERM, Some(Handler), 0), Pend);
    assert_eq!(tkill_action(TERM, Some(Handler), bit(TERM)), Pend);
    assert_eq!(tkill_action(CHLD, Some(Default), 0), Drop);
    assert_eq!(tkill_action(TERM, Some(Default), bit(TERM)), Pend);
    // A fatal default never acts inside `tkill`: pend and interrupt, and the
    // target's own return to user mode runs the exit (2026-11-11).
    assert_eq!(tkill_action(TERM, Some(Default), 0), PendInterrupt);
    // SIGUSR1 to a sibling thread with no handler kills the group, as on Linux.
    assert_eq!(tkill_action(10, Some(Default), 0), PendInterrupt);
    // Blocked signals are never discarded, whatever the disposition (Linux).
    assert_eq!(tkill_action(CHLD, Some(Default), bit(CHLD)), Pend);
    assert_eq!(tkill_action(TERM, Some(Ignore), bit(TERM)), Pend);
}

/// Exhaustive: every signal, target and blocked state, against the rules
/// stated as properties rather than as the function's own branches.
#[test]
fn exhaustive_sweep_holds_the_invariants() {
    let mut seen = [false; 3];
    for sig in 1..=64 {
        for target in targets() {
            for blocked in [false, true] {
                let mask = if blocked { bit(sig) } else { 0 };
                let a = tkill_action(sig, target, mask);
                seen[a as usize] = true;
                let ctx = (sig, target, blocked, a);
                let interrupts = a == TkillAction::PendInterrupt;
                // A thread with no process is never acted on.
                let Some(d) = target else {
                    assert_eq!(a, TkillAction::Drop, "{ctx:?}");
                    continue;
                };
                if sig == SIGKILL {
                    // SIGKILL cannot be blocked, ignored or handled.
                    assert!(interrupts, "{ctx:?}");
                    continue;
                }
                if blocked {
                    // Blocked: never lost, never an interrupt.
                    assert_eq!(a, TkillAction::Pend, "{ctx:?}");
                    continue;
                }
                // Unblocked: only a fatal default interrupts.
                assert_eq!(interrupts, d == Disposition::Default && fatal_by_default(sig), "{ctx:?}");
                if d == Disposition::Ignore {
                    assert_eq!(a, TkillAction::Drop, "{ctx:?}");
                }
                if d == Disposition::Handler {
                    assert_eq!(a, TkillAction::Pend, "{ctx:?}");
                }
            }
        }
    }
    assert!(seen.iter().all(|&s| s), "every action is reachable: {seen:?}");
}

/// Fuzz the mask: only the signal's own bit may matter. A seeded xorshift so a
/// failure reproduces.
#[test]
fn only_the_signals_own_mask_bit_matters() {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let all: std::vec::Vec<_> = targets().collect();
    for _ in 0..200_000 {
        let sig = next() % 64 + 1;
        let target = all[(next() % all.len() as u64) as usize];
        let mask = next();
        assert_eq!(
            tkill_action(sig, target, mask),
            tkill_action(sig, target, mask & bit(sig)),
            "sig {sig} target {target:?} mask {mask:#x}"
        );
    }
}
