//! A reproduction of the [`Nic::resync_rx_cursor`] reordering flaw found in
//! the driver audit — see `docs/archive/NETWORKING_AUDIT.md` §2.
//!
//! `resync_rx_cursor` scans from cursor+1 on the premise that the cursor slot
//! was chip-owned last time `receive` looked. That premise is stale by one
//! lap: the glue's resync guard (`RESYNC_AFTER_IDLE_LAPS` /
//! `RESYNC_SCAN_LAPS` in `akuma-net-nic/src/rtl8169.rs`) runs the scan
//! *before* `receive` on the scan lap, and the chip may have completed the
//! cursor slot — and the one behind it — in between. The scan then seeks past
//! the cursor slot, stranding the OLDER frame: it comes out of order (this
//! test's 0xBB before 0xAA), and the stranded frame is not reachable again
//! until the cursor wraps the whole ring.
//!
//! This test asserts the correct behaviour and therefore fails today. It is
//! `#[ignore]`d so the suite stays green until the flaw is fixed; unignore it
//! when `resync_rx_cursor` scans from the cursor itself (start at `step 0`),
//! which repairs the rewind case and the forward case with one change.

#![no_std]

use akuma_net_rtl8169::model::{FakeChip, RX_LEN};
use akuma_net_rtl8169::ring::MIN_FRAME;
use akuma_net_rtl8169::Nic;

#[test]
#[ignore = "demonstrates resync_rx_cursor stranding the frame under the cursor — docs/archive/NETWORKING_AUDIT.md §2"]
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

    // What the glue does on a scan lap, before receive.
    let _ = nic.resync_rx_cursor();

    // Both frames must come out, oldest first.
    let first = nic.receive(&mut buf).expect("both frames must be delivered");
    assert_eq!(buf[0], 0xAA, "FIFO order violated: the newer frame came out first");
    let second = nic.receive(&mut buf).expect("the stranded frame must be reachable");
    assert_eq!(buf[..second][0], 0xBB);
    assert_eq!((first, second), (MIN_FRAME, MIN_FRAME));
}
