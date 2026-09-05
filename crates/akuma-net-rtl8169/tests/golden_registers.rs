//! The register map, checked against a real chip.
//!
//! [`GOLDEN`] is a byte-for-byte dump of the 256-byte register block of the
//! RTL8168g this crate was written for, taken while Linux's own driver had it
//! **working**: link up at 1 Gbps full duplex, receiver and transmitter
//! enabled, passing traffic. It was read through a read-only mapping of BAR2
//! (`/sys/bus/pci/devices/0000:03:00.0/resource2`) without disturbing the
//! driver that owned the device.
//!
//! Every assertion below is therefore a claim about **this silicon**, not about
//! a document: if an offset in [`akuma_net_rtl8169::regs`] is wrong, or a field
//! is decoded wrongly, the value here disagrees and the test fails. That is the
//! whole point of keeping the dump rather than a summary of it.
//!
//! What it cannot check is anything the chip only does in response to a write —
//! for that, see `bringup.rs` and the simulated chip.

use akuma_net_rtl8169::chip::Model;
use akuma_net_rtl8169::link::{LinkState, Speed};
use akuma_net_rtl8169::{MacAddr, regs};

/// Registers 0x00..0x100 of a live RTL8168g, link up, passing traffic.
#[rustfmt::skip]
const GOLDEN: [u8; 256] = [
    // 0x00
    0x60, 0x02, 0x92, 0x61, 0x4e, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x02, 0x80, 0x02, 0x80, 0x00,
    // 0x10
    0x00, 0xf0, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x70, 0x0f, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x20
    0x00, 0xe0, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x30
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x2f, 0x00, 0x00, 0x00,
    // 0x40
    0x80, 0x0f, 0x00, 0x4f, 0x0e, 0xcf, 0x02, 0x00, 0xb5, 0x65, 0xb1, 0x54, 0x00, 0x00, 0x00, 0x00,
    // 0x50
    0x10, 0x00, 0xcf, 0x38, 0x60, 0x11, 0x02, 0x01, 0x11, 0x11, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x60
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x93, 0x00, 0x80, 0x30,
    // 0x70
    0x20, 0x04, 0x00, 0xff, 0xa8, 0xf1, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0d, 0x00,
    // 0x80
    0x21, 0x7c, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0x90
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0xa0
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0xb0
    0x02, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x06, 0x00, 0xe9, 0xd2, 0x00, 0x00, 0x00, 0x00,
    // 0xc0
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // 0xd0
    0x20, 0x00, 0x00, 0x32, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x73, 0xfd, 0x97, 0x00,
    // 0xe0
    0x61, 0x20, 0x00, 0x00, 0x00, 0xd0, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x27, 0x00, 0x00, 0x00,
    // 0xf0
    0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

fn g8(off: u16) -> u8 {
    GOLDEN[usize::from(off)]
}
fn g16(off: u16) -> u16 {
    let o = usize::from(off);
    u16::from_le_bytes([GOLDEN[o], GOLDEN[o + 1]])
}
fn g32(off: u16) -> u32 {
    let o = usize::from(off);
    u32::from_le_bytes([GOLDEN[o], GOLDEN[o + 1], GOLDEN[o + 2], GOLDEN[o + 3]])
}

/// The address `ip link` reports for this interface is at [`regs::IDR0`].
/// If this fails, the register block is not where we think it is at all.
#[test]
fn the_station_address_is_where_the_map_says() {
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = g8(regs::IDR0 + i as u16);
    }
    assert_eq!(MacAddr(mac).0, [0x60, 0x02, 0x92, 0x61, 0x4e, 0x73]);
    assert!(MacAddr(mac).is_plausible());
}

#[test]
fn the_revision_field_identifies_an_rtl8168g() {
    assert_eq!(Model::from_tcr(g32(regs::TCR)), Model::Rtl8168g);
}

/// `dmesg` said "Link is Up - 1Gbps/Full" when this dump was taken.
#[test]
fn the_link_decode_agrees_with_what_linux_reported() {
    let l = LinkState::from_phystatus(g8(regs::PHYSTATUS));
    assert!(l.up);
    assert_eq!(l.speed, Speed::Mb1000);
    assert!(l.full_duplex);
}

/// `ethtool -d` decoded this same byte as "Rx on, Tx on".
#[test]
fn the_command_register_shows_a_running_chip() {
    let cr = g8(regs::CR);
    assert_eq!(cr & regs::CR_RE, regs::CR_RE, "receiver should be enabled");
    assert_eq!(cr & regs::CR_TE, regs::CR_TE, "transmitter should be enabled");
    assert_eq!(cr & regs::CR_RST, 0, "a running chip is not in reset");
}

/// The interrupt mask a working driver leaves programmed. Our default is the
/// same set, and this is where that claim is checked rather than asserted in a
/// comment.
#[test]
fn the_default_interrupt_mask_matches_a_working_driver() {
    assert_eq!(g16(regs::IMR), regs::INT_DEFAULT_MASK);
    // Spelled out, so a change to either side says which bit moved.
    assert_eq!(g16(regs::IMR), 0x002F);
    assert_ne!(regs::INT_DEFAULT_MASK & regs::INT_LINKCHG, 0);
    assert_eq!(
        regs::INT_DEFAULT_MASK & regs::INT_RDU,
        0,
        "a dry receive ring is discovered by polling, not by interrupt storm"
    );
}

/// The receive filter of a host that is not in promiscuous mode.
#[test]
fn the_receive_filter_accepts_exactly_what_a_host_needs() {
    let rcr = g32(regs::RCR);
    assert_ne!(rcr & regs::RCR_APM, 0, "frames for this station");
    assert_ne!(rcr & regs::RCR_AB, 0, "broadcast");
    assert_ne!(rcr & regs::RCR_AM, 0, "multicast");
    assert_eq!(rcr & regs::RCR_AAP, 0, "not promiscuous");
    assert_eq!(rcr & regs::RCR_MXDMA_MASK, regs::RCR_MXDMA_UNLIMITED);
    // This one is why the dump is kept rather than a summary: the crate first
    // used the all-ones "no threshold" encoding here, and the live chip is
    // running 0b110. The constant was corrected to the measured value.
    assert_eq!(rcr & regs::RCR_RXFTH_MASK, regs::RCR_RXFTH_DEFAULT);
    assert_eq!(rcr & regs::RCR_RXFTH_MASK, 0xC000);
}

/// C+ mode is what makes the descriptor rings exist at all.
#[test]
fn the_chip_is_in_descriptor_ring_mode() {
    let cpcr = g16(regs::CPCR);
    assert_ne!(cpcr & regs::CPCR_NORMAL, 0);
    assert_ne!(cpcr & regs::CPCR_TXENB, 0);
}

/// Both ring bases must be 256-byte aligned, and a live chip's are — which is
/// also a check that these two offsets are the ring registers and not something
/// else that happens to hold a number.
#[test]
fn the_live_ring_bases_are_aligned() {
    let rx = u64::from(g32(regs::RDSAR)) | (u64::from(g32(regs::RDSAR + 4)) << 32);
    let tx = u64::from(g32(regs::TNPDS)) | (u64::from(g32(regs::TNPDS + 4)) << 32);
    assert_ne!(rx, 0, "a running chip has a receive ring");
    assert_ne!(tx, 0, "a running chip has a transmit ring");
    assert_eq!(rx % 256, 0, "receive ring base must be 256-byte aligned");
    assert_eq!(tx % 256, 0, "transmit ring base must be 256-byte aligned");
    assert_ne!(rx, tx, "the two rings are different arrays");
}

/// A chip that is receiving cannot have its data-valid signal gated off. This
/// pins the bit position: if [`regs::MISC_RXDV_GATED`] were wrong, a working
/// chip would appear to be gated.
#[test]
fn the_receive_gate_is_open_on_a_working_chip() {
    assert_eq!(g32(regs::MISC) & regs::MISC_RXDV_GATED, 0);
}

/// `PHYAR` is a request/response port: idle between transactions, so a live
/// chip nobody is talking to reads back with no operation outstanding.
#[test]
fn the_phy_window_is_idle() {
    assert_eq!(g32(regs::PHYAR) & akuma_net_rtl8169::mdio::BUSY, 0);
}
