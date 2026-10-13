//! A regression pin for the [`Nic::resync_rx_cursor`] reordering flaw found in
//! the driver audit — see `docs/archive/NETWORKING_AUDIT.md` §2, fixed by
//! scanning from the cursor itself rather than cursor+1.
//!
//! `resync_rx_cursor` must never move the cursor past a completed frame. When
//! the chip has completed the cursor slot *and* the one behind it — the
//! ordinary state at the head of a burst, and a state the glue's scan lap can
//! observe because its premise ("the cursor slot was chip-owned last lap") is
//! stale by one lap — the cursor is honest and the frames must come out in
//! ring order. The earlier scan sought past the cursor slot, delivering the
//! newer frame first and stranding the older one behind the cursor until a
//! full ring wrap.
//!
//! This test asserted the correct behaviour and failed before the fix; it is
//! kept green by the same property it once demonstrated the absence of.

#![no_std]

use akuma_net_rtl8169::model::{FakeChip, RX_LEN};
use akuma_net_rtl8169::ring::MIN_FRAME;
use akuma_net_rtl8169::Nic;

#[test]
fn resync_never_skips_a_frame_sitting_under_the_cursor() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let mut buf = [0u8; MIN_FRAME + 8];

    // Walk the cursor to slot RX_LEN-2 so a two-frame burst lands at 6, 7.
    for _ in 0..RX_LEN - 2 {
        assert!(chip.deliver(&[0x11; MIN_FRAME]));
        assert!(nic.receive(&mut buf).is_some());
    }
    assert_eq!(nic.rx_cursor(), RX_LEN - 2);

    // The burst: the chip completes two frames back to back — the oldest at
    // the cursor slot, a newer one right behind it. This is a legal state at
    // scan time: the previous lap's receive() saw the cursor slot chip-owned;
    // both completions happened since.
    assert!(chip.deliver(&[0xAA; MIN_FRAME]), "older frame at slot 6");
    assert!(chip.deliver(&[0xBB; MIN_FRAME]), "newer frame at slot 7");

    // What the glue does on a scan lap, before receive. With the fix the
    // cursor is honest here, so the scan must report nothing to repair.
    assert_eq!(
        nic.resync_rx_cursor(),
        None,
        "the cursor is honest when its own slot is complete: nothing to seek past"
    );

    // Both frames come out, oldest first, on consecutive calls.
    let first = nic.receive(&mut buf).expect("both frames must be delivered");
    assert_eq!(buf[0], 0xAA, "FIFO order violated: the newer frame came out first");
    let second = nic.receive(&mut buf).expect("the second frame must be reachable");
    assert_eq!(buf[..second][0], 0xBB);
    assert_eq!((first, second), (MIN_FRAME, MIN_FRAME));
}
