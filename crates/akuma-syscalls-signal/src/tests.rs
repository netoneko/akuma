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
