//! The RTL8169/8168/8111 register block.
//!
//! Offsets are Realtek's, in the vendor's own names (`TNPDS`, `RDSAR`, `CPCR`,
//! `TPPoll`, `RMS`, `MTPS`, …) rather than any driver's spelling of them, and
//! every value here was **read back off a live RTL8168g** before it was written
//! down — see `tests/golden_registers.rs`, which holds the full 256-byte dump of
//! a working chip and asserts this map against it.
//!
//! Only the first 256 bytes are described. That block is the whole standard
//! register file; everything past it is per-model and this driver does not go
//! there.
//!
//! Widths matter and are not free choices. `CR` is a byte, `IMR`/`ISR`/`CPCR`
//! are halfwords, `TCR`/`RCR`/`PHYAR` are words, and the descriptor-base
//! registers are 64-bit values written as two words, low half first. A 32-bit
//! write to a byte register is not a wider version of the same operation — it
//! lands on three neighbours as well.

/// Station address, 6 bytes at 0x00..0x06.
///
/// Read-only until `CR9346` is put in config-write mode; this driver only reads
/// it, taking whatever the EEPROM loaded at power-on as the MAC.
pub const IDR0: u16 = 0x00;

/// Multicast hash table, 8 bytes. All-ones accepts every multicast group.
pub const MAR0: u16 = 0x08;

/// Dump-tally-counter command, 64-bit. Writing a physical address with bit 3
/// set asks the chip to DMA its statistics block there. Unused here.
pub const DTCCR: u16 = 0x10;

/// Transmit Normal Priority Descriptor Start, 64-bit, **256-byte aligned**.
pub const TNPDS: u16 = 0x20;

/// Transmit High Priority Descriptor Start, 64-bit, 256-byte aligned.
pub const THPDS: u16 = 0x28;

/// Command register (byte). Reset, and the receiver/transmitter enables.
pub const CR: u16 = 0x37;

/// Transmit Priority Polling (byte) — the doorbell. Writing [`TPPOLL_NPQ`]
/// tells the chip to re-scan the normal-priority ring.
pub const TPPOLL: u16 = 0x38;

/// Interrupt Mask Register (halfword).
pub const IMR: u16 = 0x3C;

/// Interrupt Status Register (halfword), **write-1-to-clear**.
pub const ISR: u16 = 0x3E;

/// Transmit Configuration Register (word). Also carries the hardware revision
/// in read-only bits — see [`crate::chip`].
pub const TCR: u16 = 0x40;

/// Receive Configuration Register (word): the accept filter, DMA burst and
/// FIFO threshold.
pub const RCR: u16 = 0x44;

/// Missed-packet counter (word), cleared by writing zero.
pub const MPC: u16 = 0x4C;

/// 93C46 EEPROM command register (byte). Its top two bits gate writes to the
/// `CONFIG*` registers and to `IDR0`.
pub const CR9346: u16 = 0x50;

/// CONFIG0..CONFIG5, one byte each at 0x51..0x57.
pub const CONFIG0: u16 = 0x51;
/// See [`CONFIG0`]. `CONFIG1` bit 5 is the "driver loaded" flag.
pub const CONFIG1: u16 = 0x52;
/// See [`CONFIG0`].
pub const CONFIG2: u16 = 0x53;
/// See [`CONFIG0`].
pub const CONFIG3: u16 = 0x54;
/// See [`CONFIG0`].
pub const CONFIG4: u16 = 0x55;
/// See [`CONFIG0`].
pub const CONFIG5: u16 = 0x56;

/// Timer interrupt register (word) on gigabit parts.
pub const TIMERINT: u16 = 0x58;

/// PHY Access Register (word) — the MDIO window. See [`crate::mdio`].
pub const PHYAR: u16 = 0x60;

/// PHY status (byte): link, duplex and negotiated speed. See [`crate::link`].
pub const PHYSTATUS: u16 = 0x6C;

/// Receive Packet Maximum Size (halfword). Frames longer than this are dropped
/// by the receiver.
pub const RMS: u16 = 0xDA;

/// C+ Command Register (halfword): selects the descriptor-ring datapath.
///
/// **Must be programmed before the ring base addresses.** That is the one
/// ordering constraint in the whole bring-up that the register names do not
/// hint at, and getting it backwards leaves the bases where the chip is not
/// looking.
pub const CPCR: u16 = 0xE0;

/// Receive Descriptor Start Address, 64-bit, 256-byte aligned.
pub const RDSAR: u16 = 0xE4;

/// Max Transmit Packet Size (byte), in units of 128 bytes.
pub const MTPS: u16 = 0xEC;

/// Miscellaneous control (word) on 8168-class parts. Bit 19 gates RXDV.
pub const MISC: u16 = 0xF0;

// ---------------------------------------------------------------------------
// CR — command register
// ---------------------------------------------------------------------------

/// Software reset. Write it, then poll until the chip clears it.
pub const CR_RST: u8 = 0x10;
/// Receiver enable.
pub const CR_RE: u8 = 0x08;
/// Transmitter enable.
pub const CR_TE: u8 = 0x04;

// ---------------------------------------------------------------------------
// TPPoll — the transmit doorbell
// ---------------------------------------------------------------------------

/// Poll the high-priority transmit queue.
pub const TPPOLL_HPQ: u8 = 0x80;
/// Poll the normal-priority transmit queue.
pub const TPPOLL_NPQ: u8 = 0x40;
/// Raise a software interrupt.
pub const TPPOLL_FSWINT: u8 = 0x01;

// ---------------------------------------------------------------------------
// ISR / IMR — the same bit layout in both
// ---------------------------------------------------------------------------

/// A frame was received.
pub const INT_ROK: u16 = 0x0001;
/// A receive error was counted.
pub const INT_RER: u16 = 0x0002;
/// A frame was transmitted.
pub const INT_TOK: u16 = 0x0004;
/// A transmit error was counted.
pub const INT_TER: u16 = 0x0008;
/// Receive descriptor unavailable — the ring ran dry.
pub const INT_RDU: u16 = 0x0010;
/// Link state changed.
pub const INT_LINKCHG: u16 = 0x0020;
/// Receive FIFO overflowed.
pub const INT_RXOVW: u16 = 0x0040;
/// Transmit descriptor unavailable.
pub const INT_TDU: u16 = 0x0080;
/// Software interrupt, raised via [`TPPOLL_FSWINT`].
pub const INT_SWINT: u16 = 0x0100;
/// The moderation timer expired.
pub const INT_TIMEOUT: u16 = 0x4000;
/// System error — a failed bus transaction. Always fatal.
pub const INT_SERR: u16 = 0x8000;

/// What this driver asks to be interrupted about.
///
/// Deliberately the same set the Linux driver leaves in `IMR` on a live link
/// (observed `0x002f` on the reference chip): the four completion/error bits
/// plus link change. `INT_RDU` is **not** in it — a dry receive ring is a
/// condition the poll loop discovers on its own, and enabling it on a busy
/// link produces an interrupt storm rather than information.
pub const INT_DEFAULT_MASK: u16 =
    INT_ROK | INT_RER | INT_TOK | INT_TER | INT_LINKCHG;

// ---------------------------------------------------------------------------
// CR9346 — config register write gate
// ---------------------------------------------------------------------------

/// Normal operation: `CONFIG*` and `IDR0` are read-only.
pub const CR9346_LOCK: u8 = 0x00;
/// Config-register write enable. Both top bits set.
pub const CR9346_UNLOCK: u8 = 0xC0;

// ---------------------------------------------------------------------------
// CONFIG1
// ---------------------------------------------------------------------------

/// "A driver is loaded" — some board firmware watches this bit.
pub const CONFIG1_DRVLOAD: u8 = 0x20;

// ---------------------------------------------------------------------------
// TCR — transmit config
// ---------------------------------------------------------------------------

/// The read-only hardware-revision field. See [`crate::chip::Model`].
pub const TCR_HWREV_MASK: u32 = 0x7CF0_0000;
/// Maximum DMA burst per transmit, bits 8..11.
pub const TCR_MXDMA_MASK: u32 = 0x0000_0700;
/// Unlimited transmit DMA burst.
pub const TCR_MXDMA_UNLIMITED: u32 = 0x0000_0700;
/// Interframe gap field, bits 24..26. The value below is the IEEE-standard gap.
pub const TCR_IFG_MASK: u32 = 0x0300_0000;
/// Standard 9.6 µs interframe gap.
pub const TCR_IFG_STANDARD: u32 = 0x0300_0000;

// ---------------------------------------------------------------------------
// RCR — receive config
// ---------------------------------------------------------------------------

/// Accept every frame on the wire (promiscuous).
pub const RCR_AAP: u32 = 0x0000_0001;
/// Accept frames addressed to this station.
pub const RCR_APM: u32 = 0x0000_0002;
/// Accept multicast that passes the hash filter.
pub const RCR_AM: u32 = 0x0000_0004;
/// Accept broadcast.
pub const RCR_AB: u32 = 0x0000_0008;
/// Accept runts (undersized frames).
pub const RCR_AR: u32 = 0x0000_0010;
/// Accept frames with errors.
pub const RCR_AER: u32 = 0x0000_0020;
/// Maximum DMA burst per receive, bits 8..11.
pub const RCR_MXDMA_MASK: u32 = 0x0000_0700;
/// Unlimited receive DMA burst.
pub const RCR_MXDMA_UNLIMITED: u32 = 0x0000_0700;
/// Receive FIFO threshold, bits 13..16.
pub const RCR_RXFTH_MASK: u32 = 0x0000_E000;

/// The receive FIFO threshold a working driver leaves programmed.
///
/// **Measured, not assumed.** The all-ones encoding of this field is commonly
/// described as "no threshold, start the DMA immediately", and this crate
/// originally used it — but the reference chip, running at a gigabit and
/// passing traffic under Linux, has `0b110` here, not `0b111`. The fixture in
/// `tests/golden_registers.rs` is what caught that, and the measurement wins:
/// a value a chip of this exact revision is known to run at is worth more than
/// an encoding table's idea of the most aggressive setting.
pub const RCR_RXFTH_DEFAULT: u32 = 0x0000_C000;

// ---------------------------------------------------------------------------
// CPCR — C+ mode
// ---------------------------------------------------------------------------

/// Enable the descriptor-ring transmit path.
pub const CPCR_TXENB: u16 = 0x0001;
/// Enable the descriptor-ring receive path. 8168-class parts leave this clear
/// and take the receive path from `CR.RE` instead.
pub const CPCR_RXENB: u16 = 0x0002;
/// Allow PCI multi-read/write.
pub const CPCR_MULRW: u16 = 0x0008;
/// Offload receive checksums.
pub const CPCR_RXCSUM: u16 = 0x0020;
/// Strip VLAN tags in hardware.
pub const CPCR_VLANSTRIP: u16 = 0x0040;
/// Reserved-but-required on 8168-class parts; the reference chip has it set.
pub const CPCR_NORMAL: u16 = 0x2000;

// ---------------------------------------------------------------------------
// MISC
// ---------------------------------------------------------------------------

/// While set, the receiver's data-valid signal is gated off and no frame can
/// arrive. Some parts come out of reset with it set.
pub const MISC_RXDV_GATED: u32 = 0x0008_0000;
