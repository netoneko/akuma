//! Host tests for the ring.
//!
//! The arithmetic here is the whole crate, and the test that matters most is
//! [`chunked_drain_delivers_the_whole_ring`] — the shape of the bug that made a
//! 64 KiB `dmesg` silently return its last 4 KiB.

extern crate std;

use std::vec::Vec;

use super::{Action, Ring};

/// Read everything out of `r` through a staging buffer of `chunk` bytes, the way
/// `sys_syslog` does, and return it. This is the *caller* side of the contract,
/// written once so several tests can assert against it.
fn drain(r: &Ring<{ 4 * 1024 }>, chunk: usize, ceiling: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stage = Vec::new();
    stage.resize(chunk, 0u8);
    let want = ceiling.min(r.len());
    while out.len() < want {
        let take = (want - out.len()).min(chunk);
        let n = r.snapshot_from(out.len(), &mut stage[..take]);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&stage[..n]);
    }
    out
}

#[test]
fn empty_ring_reports_nothing() {
    let r = Ring::<64>::new();
    assert_eq!(r.len(), 0);
    assert!(r.is_empty());
    assert_eq!(r.total(), 0);
    assert_eq!(r.dropped(), 0);
    let mut out = [0u8; 8];
    assert_eq!(r.snapshot_from(0, &mut out), 0);
}

#[test]
fn under_capacity_reads_back_verbatim() {
    let mut r = Ring::<64>::new();
    r.push_bytes(b"hello");
    assert_eq!(r.len(), 5);
    assert_eq!(r.total(), 5);
    assert_eq!(r.dropped(), 0);
    let mut out = [0u8; 16];
    let n = r.snapshot_from(0, &mut out);
    assert_eq!(&out[..n], b"hello");
}

#[test]
fn exactly_full_reads_back_verbatim() {
    let mut r = Ring::<8>::new();
    r.push_bytes(b"abcdefgh");
    assert_eq!(r.len(), 8);
    assert_eq!(r.dropped(), 0);
    let mut out = [0u8; 8];
    assert_eq!(r.snapshot_from(0, &mut out), 8);
    assert_eq!(&out, b"abcdefgh");
}

#[test]
fn overflow_keeps_the_newest_and_reports_the_loss() {
    let mut r = Ring::<8>::new();
    r.push_bytes(b"abcdefghij"); // 10 bytes into an 8-byte ring
    assert_eq!(r.len(), 8);
    assert_eq!(r.total(), 10);
    assert_eq!(r.dropped(), 2, "a,b were evicted");
    let mut out = [0u8; 8];
    assert_eq!(r.snapshot_from(0, &mut out), 8);
    assert_eq!(&out, b"cdefghij", "oldest retrievable byte first");
}

/// Many wraps, so the cursor has gone round the buffer repeatedly rather than
/// just once — the case where an off-by-one in `dropped()` stops being masked by
/// a small `total`.
#[test]
fn many_wraps_still_reads_the_last_cap_bytes() {
    let mut r = Ring::<8>::new();
    for i in 0u8..200 {
        r.push(i);
    }
    assert_eq!(r.total(), 200);
    assert_eq!(r.len(), 8);
    assert_eq!(r.dropped(), 192);
    let mut out = [0u8; 8];
    assert_eq!(r.snapshot_from(0, &mut out), 8);
    assert_eq!(out, [192, 193, 194, 195, 196, 197, 198, 199]);
}

#[test]
fn skip_walks_forward_through_history() {
    let mut r = Ring::<16>::new();
    r.push_bytes(b"0123456789");
    let mut out = [0u8; 4];
    assert_eq!(r.snapshot_from(0, &mut out), 4);
    assert_eq!(&out, b"0123");
    assert_eq!(r.snapshot_from(4, &mut out), 4);
    assert_eq!(&out, b"4567");
    assert_eq!(r.snapshot_from(8, &mut out), 2);
    assert_eq!(&out[..2], b"89");
}

/// A `skip` at or past the end returns 0 rather than wrapping around to the
/// start — the property the `while n != 0` drain loop terminates on.
#[test]
fn skip_past_the_end_returns_zero() {
    let mut r = Ring::<16>::new();
    r.push_bytes(b"0123456789");
    let mut out = [0u8; 4];
    assert_eq!(r.snapshot_from(10, &mut out), 0, "skip == len");
    assert_eq!(r.snapshot_from(11, &mut out), 0, "one past");
    assert_eq!(r.snapshot_from(usize::MAX, &mut out), 0, "absurdly past");
}

/// **The regression this crate exists for.**
///
/// `sys_syslog` staged through a 4 KiB buffer while `SIZE_BUFFER` advertised the
/// whole ring. Reading in one pass returned the last 4 KiB and reported no
/// error; `busybox dmesg` printed a fraction of the boot with nothing saying so.
/// A `skip`-driven loop must deliver every byte, in order, whatever the staging
/// size is.
#[test]
fn chunked_drain_delivers_the_whole_ring() {
    const CAP: usize = 4 * 1024;
    let mut r = Ring::<CAP>::new();
    // Distinguishable, position-dependent content: a wrong offset shows up as
    // wrong bytes, not just a wrong length.
    let filled: Vec<u8> = (0..CAP).map(|i| (i % 251) as u8).collect();
    r.push_bytes(&filled);
    assert_eq!(r.len(), CAP);

    for chunk in [1, 7, 64, 512, CAP - 1, CAP, CAP + 1] {
        let got = drain(&r, chunk, CAP);
        assert_eq!(got.len(), CAP, "chunk={chunk} delivered a short log");
        assert_eq!(got, filled, "chunk={chunk} delivered the wrong bytes");
    }
}

/// The same drain, but against a ring that has wrapped — so the chunk boundaries
/// and the wrap point are not aligned.
#[test]
fn chunked_drain_across_a_wrap() {
    const CAP: usize = 4 * 1024;
    let mut r = Ring::<CAP>::new();
    let written: Vec<u8> = (0..CAP + 1234).map(|i| (i % 251) as u8).collect();
    r.push_bytes(&written);
    let expected = &written[1234..];
    assert_eq!(r.len(), CAP);
    assert_eq!(r.dropped(), 1234);

    for chunk in [1, 13, 100, 4096] {
        let got = drain(&r, chunk, CAP);
        assert_eq!(got, expected, "chunk={chunk}");
    }
}

/// A caller asking for less than the ring holds gets the *oldest* end, and the
/// loop still terminates. (`dmesg` with a small `-s`.)
#[test]
fn a_bounded_request_stops_at_its_ceiling() {
    const CAP: usize = 4 * 1024;
    let mut r = Ring::<CAP>::new();
    let filled: Vec<u8> = (0..CAP).map(|i| (i % 251) as u8).collect();
    r.push_bytes(&filled);
    let got = drain(&r, 512, 1000);
    assert_eq!(got.len(), 1000);
    assert_eq!(got, filled[..1000]);
}

#[test]
fn clear_resets_length_and_loss() {
    let mut r = Ring::<8>::new();
    r.push_bytes(b"abcdefghij");
    assert_eq!(r.dropped(), 2);
    r.clear();
    assert_eq!(r.len(), 0);
    assert_eq!(r.total(), 0);
    assert_eq!(r.dropped(), 0, "a deliberate clear is not a loss");
    let mut out = [0u8; 8];
    assert_eq!(r.snapshot_from(0, &mut out), 0);
    // And it is usable again afterwards.
    r.push_bytes(b"xy");
    assert_eq!(r.snapshot_from(0, &mut out), 2);
    assert_eq!(&out[..2], b"xy");
}

#[test]
fn push_str_crlf_expands_newlines() {
    let mut r = Ring::<32>::new();
    r.push_str_crlf("a\nb\n");
    let mut out = [0u8; 32];
    let n = r.snapshot_from(0, &mut out);
    assert_eq!(&out[..n], b"a\r\nb\r\n");
}

/// A `\r\n` already in the source must not become `\r\r\n`: the expansion keys
/// on the `\n` alone, and a driver that emits both bytes hands them to `push`
/// rather than here.
#[test]
fn push_str_crlf_does_not_double_an_existing_cr() {
    let mut r = Ring::<32>::new();
    r.push_str_crlf("a\r\n");
    let mut out = [0u8; 32];
    let n = r.snapshot_from(0, &mut out);
    assert_eq!(&out[..n], b"a\r\r\n", "documented: it expands every \\n it sees");
}

// ---------------------------------------------------------------------------
// The disabled ring
// ---------------------------------------------------------------------------

/// `CAP == 0` must not divide by zero, must not panic, and must answer
/// consistently — this is the whole "optional" story, and every one of these
/// operations reaches a `% CAP` in the enabled path.
#[test]
fn zero_capacity_ring_is_a_working_no_op() {
    let mut r = Ring::<0>::new();
    assert_eq!(r.capacity(), 0);
    r.push(b'x');
    r.push_bytes(b"a whole line of diagnostics\n");
    r.push_str_crlf("and another\n");
    assert_eq!(r.len(), 0);
    assert!(r.is_empty());
    assert_eq!(r.total(), 0, "a disabled ring reports no history, not a loss");
    assert_eq!(r.dropped(), 0);
    let mut out = [0u8; 8];
    assert_eq!(r.snapshot_from(0, &mut out), 0);
    assert_eq!(r.snapshot_from(4, &mut out), 0);
    r.clear();
    assert_eq!(r.len(), 0);
}

/// The drain loop a `sys_syslog` runs must terminate against a disabled ring
/// rather than spinning on a zero-length read it keeps retrying.
#[test]
fn zero_capacity_ring_terminates_a_drain_loop() {
    let r = Ring::<0>::new();
    let mut stage = [0u8; 64];
    let mut done = 0usize;
    let mut laps = 0;
    while done < r.len() {
        let n = r.snapshot_from(done, &mut stage);
        laps += 1;
        assert!(laps < 10, "drain loop did not terminate");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(done, 0);
}

/// `Ring<0>` costs the counter and nothing else — the buffer really is gone,
/// not merely unused. This is the number the "optional" claim rests on, so it is
/// pinned rather than asserted in prose: `Ring<0>` is 8 bytes (the `u64`), not
/// zero, and not `CAP`.
#[test]
fn zero_capacity_ring_costs_only_the_counter() {
    use core::mem::size_of;
    assert_eq!(size_of::<Ring<0>>(), size_of::<u64>());
    assert_eq!(size_of::<Ring<8>>(), size_of::<u64>() + 8);
    assert_eq!(size_of::<Ring<{ 64 * 1024 }>>(), size_of::<u64>() + 64 * 1024);
}

/// A one-byte ring: `CAP == 1` wraps on every push, so an off-by-one in the
/// cursor is maximally visible.
#[test]
fn one_byte_ring_keeps_the_last_byte() {
    let mut r = Ring::<1>::new();
    r.push_bytes(b"abc");
    assert_eq!(r.len(), 1);
    assert_eq!(r.dropped(), 2);
    let mut out = [0u8; 4];
    assert_eq!(r.snapshot_from(0, &mut out), 1);
    assert_eq!(out[0], b'c');
    assert_eq!(r.snapshot_from(1, &mut out), 0);
}

// ---------------------------------------------------------------------------
// syslog(2) action decode
// ---------------------------------------------------------------------------

#[test]
fn every_known_action_decodes() {
    use Action::{
        Clear, Close, ConsoleLevel, ConsoleOff, ConsoleOn, Open, Read, ReadAll, ReadClear,
        SizeBuffer, SizeUnread,
    };
    let expect = [
        (0, Close),
        (1, Open),
        (2, Read),
        (3, ReadAll),
        (4, ReadClear),
        (5, Clear),
        (6, ConsoleOff),
        (7, ConsoleOn),
        (8, ConsoleLevel),
        (9, SizeUnread),
        (10, SizeBuffer),
    ];
    for (n, want) in expect {
        assert_eq!(Action::decode(n), Some(want), "action {n}");
    }
}

#[test]
fn unknown_actions_are_einval() {
    for n in [11u64, 12, 99, u64::MAX] {
        assert_eq!(Action::decode(n), None, "action {n} should be EINVAL");
    }
}

/// The classification predicates are what a `sys_syslog` branches on, so every
/// action must fall into exactly one arm of the `match` a caller writes — no
/// action unhandled, none handled twice.
///
/// There are **four** arms, not three: `Clear` reads nothing and returns no
/// size, so `reads`/`sizes`/`is_noop` do not cover it. `clears()` is the
/// orthogonal one — `ReadClear` both reads and clears, and an implementation
/// that treats it as exclusive drops the log on the floor before copying it out.
#[test]
fn action_classification_is_consistent() {
    for n in 0u64..=10 {
        let a = Action::decode(n).unwrap();
        let pure_clear = a.clears() && !a.reads();
        let kinds =
            u32::from(a.reads()) + u32::from(a.sizes()) + u32::from(a.is_noop()) + u32::from(pure_clear);
        assert_eq!(kinds, 1, "{a:?} must be exactly one of read/size/no-op/clear");
    }
    assert!(Action::ReadClear.reads() && Action::ReadClear.clears());
    assert!(Action::Clear.clears() && !Action::Clear.reads());
    assert!(!Action::ReadAll.clears());
    assert!(!Action::Read.clears());
}
