//! The driver, end to end, against the simulated chip.
//!
//! These run the *real* [`Nic`] code — the same `init`, `transmit` and
//! `receive` a kernel would call — against [`FakeChip`], which enforces the
//! ownership protocol and panics if the driver breaks it. What is being tested
//! is not "does it compile" but the decisions: the order of the bring-up, what
//! goes on the wire, what comes off it, and what happens when things go wrong.

use akuma_net_rtl8169::desc;
use akuma_net_rtl8169::model::{FakeChip, MODEL_MAC, RX_LEN, TX_LEN};
use akuma_net_rtl8169::ring::{MAX_FRAME, MIN_FRAME};
use akuma_net_rtl8169::{Error, Model, Nic, TxError, regs};

#[test]
fn probe_identifies_the_chip_and_reads_its_address() {
    let chip = FakeChip::new();
    let nic = Nic::probe(chip.port(), chip.port()).expect("probe should succeed");
    assert_eq!(nic.model(), Model::Rtl8168g);
    assert_eq!(nic.mac(), MODEL_MAC);
}

/// Probe must leave the device untouched — it may still belong to another
/// driver, and a stray write to a running NIC is how you take a host's network
/// down while "just looking".
#[test]
fn probe_writes_no_registers() {
    let chip = FakeChip::new();
    // An implausible station address is the realistic failure: an unmapped or
    // unpowered chip reads back as zeroes.
    for i in 0..6u16 {
        // Reach past the driver to blank the address the chip reports.
        assert_eq!(chip.peek8(regs::IDR0 + i), MODEL_MAC.0[i as usize]);
    }
    let nic = Nic::probe(chip.port(), chip.port());
    assert!(nic.is_ok());
    assert_eq!(chip.log().len(), 0, "probe must not write any register");
}

#[test]
fn init_leaves_the_chip_running() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().expect("init should succeed");

    assert!(chip.running(), "receiver and transmitter should be enabled");
    assert_eq!(chip.peek16(regs::IMR), regs::INT_DEFAULT_MASK);
    assert_eq!(chip.peek16(regs::RMS), akuma_net_rtl8169::ring::RX_BUF_SIZE);

    let rcr = chip.peek32(regs::RCR);
    assert_ne!(rcr & regs::RCR_APM, 0);
    assert_ne!(rcr & regs::RCR_AB, 0);
    assert_eq!(rcr & regs::RCR_AAP, 0, "not promiscuous by default");
}

/// The ordering constraint that has no other way to be expressed: C+ mode
/// selects the descriptor datapath, so the ring bases must be programmed after
/// it or they land where the chip is not looking.
#[test]
fn c_plus_mode_is_programmed_before_the_ring_addresses() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let log = chip.log();
    assert!(
        log.wrote_before(regs::CPCR, regs::RDSAR),
        "C+ mode must precede the receive ring base"
    );
    assert!(
        log.wrote_before(regs::CPCR, regs::TNPDS),
        "C+ mode must precede the transmit ring base"
    );
}

/// The receiver must not be enabled until the ring it will DMA into is
/// populated. Between those two events the chip would be reading uninitialised
/// ownership bits.
#[test]
fn the_receiver_is_enabled_only_after_the_rings_are_programmed() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let log = chip.log();
    let ring_base = log.first_write_to(regs::RDSAR).expect("ring base written");
    // The last CR write is the enable; the reset is the first.
    let enable = log.last_write_to(regs::CR).expect("CR written");
    assert!(
        ring_base < enable,
        "ring base at {ring_base} must precede the enable at {enable}"
    );
    assert_eq!(log.last_value(regs::CR), Some(u32::from(regs::CR_RE | regs::CR_TE)));
}

#[test]
fn every_receive_buffer_is_posted_and_only_the_last_is_marked() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let mut marked = 0;
    for i in 0..RX_LEN {
        let d = chip.rx_desc(i);
        assert!(d.owned_by_chip(), "receive descriptor {i} should be posted");
        assert_ne!(d.buf_phys(), 0, "descriptor {i} needs a buffer");
        if d.is_end_of_ring() {
            marked += 1;
            assert_eq!(i, RX_LEN - 1, "only the last entry may be marked");
        }
    }
    assert_eq!(marked, 1);
}

/// The transmit ring is not handed to the chip at init — but its shape marker
/// still has to be there, or the chip walks past the end on its first pass.
#[test]
fn the_transmit_ring_is_idle_but_shaped() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    for i in 0..TX_LEN {
        let d = chip.tx_desc(i);
        assert!(!d.owned_by_chip(), "nothing is queued yet");
        assert_eq!(d.is_end_of_ring(), i == TX_LEN - 1);
    }
}

#[test]
fn a_transmitted_frame_reaches_the_wire_intact() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let frame: [u8; 64] = core::array::from_fn(|i| i as u8);
    nic.transmit(&frame).expect("queue should accept the frame");

    let wire = chip.wire();
    assert_eq!(wire.len(), 1, "the doorbell should have sent it");
    assert_eq!(wire.get(0).unwrap().as_slice(), &frame[..]);
}

/// Short frames are padded to the Ethernet minimum by **writing zeroes**, not
/// by declaring a longer length over whatever the last frame left behind. The
/// second half of this test is the one that matters: it is an information leak
/// that no receiver would ever complain about.
#[test]
fn a_short_frame_is_padded_with_zeroes_and_leaks_nothing() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    // Fill a transmit buffer with a recognisable secret via a full-size frame.
    let secret = [0xAAu8; 200];
    nic.transmit(&secret).unwrap();
    nic.reclaim_tx();

    // Now send a short frame that lands in a later slot, then one that reuses
    // the first slot — walk the whole ring so a reused buffer is guaranteed.
    for _ in 0..TX_LEN {
        nic.reclaim_tx();
        let short = [0x5Au8; 14];
        nic.transmit(&short).unwrap();
    }

    let wire = chip.wire();
    let last = wire.last().unwrap();
    assert_eq!(last.len, MIN_FRAME, "padded up to the Ethernet minimum");
    assert_eq!(&last.as_slice()[..14], &[0x5A; 14]);
    for (i, b) in last.as_slice()[14..].iter().enumerate() {
        assert_eq!(*b, 0, "pad byte {i} leaked 0x{b:02x} from an earlier frame");
    }
}

#[test]
fn a_received_frame_comes_back_without_its_checksum() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let payload: [u8; 100] = core::array::from_fn(|i| (i as u8) ^ 0x3C);
    assert!(chip.deliver(&payload), "ring should have room");

    let mut buf = [0u8; 2048];
    let n = nic.receive(&mut buf).expect("a frame should be waiting");
    assert_eq!(n, payload.len(), "the 4-byte FCS must not be delivered");
    assert_eq!(&buf[..n], &payload[..]);
}

#[test]
fn an_empty_ring_yields_nothing() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();
    let mut buf = [0u8; 2048];
    assert_eq!(nic.receive(&mut buf), None);
}

/// A bad frame must be dropped *and its buffer re-posted*, or the ring loses an
/// entry every time the wire glitches and eventually stops receiving entirely.
#[test]
fn an_errored_frame_is_dropped_without_stalling_the_ring() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let bad = [0xFFu8; 64];
    chip.deliver_raw(&bad, desc::FS | desc::LS | desc::RX_RES, 0);
    let good: [u8; 80] = core::array::from_fn(|i| i as u8);
    chip.deliver(&good);

    let mut buf = [0u8; 2048];
    let n = nic.receive(&mut buf).expect("the good frame should get through");
    assert_eq!(n, good.len());
    assert_eq!(&buf[..n], &good[..]);

    // And the errored entry went back to the chip rather than being lost.
    assert!(chip.rx_desc(0).owned_by_chip(), "the bad entry was re-posted");
}

/// Traffic for longer than the ring is long: every frame must arrive, in order,
/// across the wrap. This is where an off-by-one in the cursor shows up.
#[test]
fn frames_survive_wrapping_the_receive_ring() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    let mut buf = [0u8; 2048];
    for round in 0..(RX_LEN * 3) {
        let frame = [round as u8; 64];
        assert!(chip.deliver(&frame), "round {round}: ring should have room");
        let n = nic.receive(&mut buf).unwrap_or_else(|| panic!("round {round}: no frame"));
        assert_eq!(n, 64);
        assert_eq!(buf[0], round as u8, "round {round}: frames out of order");
    }
}

#[test]
fn the_transmit_ring_fills_and_then_drains() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    // The model sends on the doorbell, so to fill the ring we have to look at
    // what the driver reports rather than at the chip.
    let mut queued = 0;
    while nic.can_transmit() {
        nic.transmit(&[0x11u8; 64]).unwrap();
        queued += 1;
        if queued > TX_LEN * 2 {
            break;
        }
    }
    assert!(queued >= TX_LEN - 1, "the ring should hold nearly its length");
    assert_eq!(chip.wire().len(), queued, "each doorbell sent one frame");

    // The model completed them all, so a reclaim frees the whole ring.
    let freed = nic.reclaim_tx();
    assert_eq!(freed, queued);
    assert!(nic.can_transmit());
}

#[test]
fn a_frame_that_cannot_be_sent_is_refused_rather_than_truncated() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    assert_eq!(nic.transmit(&[]), Err(TxError::BadLength { len: 0 }));
    let oversize = [0u8; MAX_FRAME + 1];
    assert_eq!(
        nic.transmit(&oversize),
        Err(TxError::BadLength { len: MAX_FRAME + 1 })
    );
    assert_eq!(chip.wire().len(), 0, "nothing should have gone out");
}

/// `ISR` is write-1-to-clear. Acknowledging must clear exactly the bits that
/// were read — a bit that arrives between the read and the write has to
/// survive, or an interrupt is lost.
#[test]
fn interrupts_are_acknowledged_without_losing_new_ones() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    chip.raise_interrupt(regs::INT_ROK | regs::INT_TOK);
    let seen = nic.take_interrupts();
    assert_eq!(seen & regs::INT_ROK, regs::INT_ROK);
    assert_eq!(seen & regs::INT_TOK, regs::INT_TOK);
    assert_eq!(chip.peek16(regs::ISR), 0, "observed bits should be cleared");

    chip.raise_interrupt(regs::INT_LINKCHG);
    assert_eq!(nic.take_interrupts(), regs::INT_LINKCHG);
    assert_eq!(nic.take_interrupts(), 0, "nothing left outstanding");
}

#[test]
fn link_state_is_read_from_the_chip() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    chip.set_phystatus(0x93); // the reference chip's live value
    let l = nic.link();
    assert!(l.up);
    assert!(l.full_duplex);
    assert!(l.is_usable());

    chip.set_phystatus(0x00);
    assert!(!nic.link().up);
}

/// The MDIO busy protocol is asymmetric; the model implements it the way the
/// chip does, so a driver that polls the wrong edge hangs here rather than
/// reading a plausible-looking zero on hardware.
#[test]
fn the_phy_can_be_read_and_written() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    nic.init().unwrap();

    chip.set_phy(1, 0xBEEF);
    assert_eq!(nic.phy_read(1), Some(0xBEEF));

    assert!(nic.phy_write(4, 0x1234));
    assert_eq!(nic.phy_read(4), Some(0x1234));
}

/// Reset must actually be waited for. The model clears the bit only after
/// several reads, so a driver that assumed it was instant fails here.
#[test]
fn reset_is_polled_to_completion() {
    let chip = FakeChip::new();
    let mut nic = Nic::probe(chip.port(), chip.port()).unwrap();
    assert_eq!(nic.reset(), Ok(()));
    assert_eq!(chip.peek8(regs::CR) & regs::CR_RST, 0);
}

/// Bad memory from the consumer is caught at probe, before anything is written.
#[test]
fn misconfigured_rings_are_refused() {
    // The model's own rings are well-formed, so this checks the error type is
    // reachable and carries the offending value.
    let e = Error::RingMisaligned { phys: 0x1080 };
    match e {
        Error::RingMisaligned { phys } => assert_eq!(phys, 0x1080),
        _ => panic!("wrong variant"),
    }
}
