//! Bring-up, transmit and receive.
//!
//! [`Nic`] is the whole driver: it owns a [`Regs`] and a [`Rings`], two ring
//! cursors and the identity it read at probe time. Everything it does is a
//! sequence of trait calls, which is why the same code runs against real
//! silicon and against [`crate::model::FakeChip`].
//!
//! # The order in [`Nic::init`] is not arbitrary
//!
//! Three constraints are real, and each is a bug that does not announce itself:
//!
//! 1. **[`regs::CPCR`] before the ring base addresses.** The C+ mode bit selects
//!    the descriptor-ring datapath. Programming ring bases while the chip is
//!    still in the legacy datapath leaves them where the mode switch does not
//!    look, and the chip runs with whatever was there before.
//! 2. **Descriptors before the base addresses, base addresses before
//!    [`regs::CR_RE`].** The moment the receiver is enabled the chip may DMA
//!    into whatever the base points at. Enabling it before the ring is
//!    populated hands it a ring of stale entries — the ownership bit in
//!    uninitialised memory is a coin flip.
//! 3. **[`regs::CR9346`] unlocked around the `CONFIG*` writes.** Outside that
//!    window those registers ignore writes silently.

use crate::chip::Model;
use crate::desc::Desc;
use crate::link::LinkState;
use crate::ring::{MAX_FRAME, MIN_FRAME, RX_BUF_SIZE, RxRing, TxRing};
use crate::{Error, MacAddr, Regs, Rings, TxError, mdio, regs};

/// How many times a bounded poll re-reads before giving up.
///
/// With [`POLL_DELAY_US`] this is a 10 ms ceiling. The reset it guards
/// completes in microseconds on working silicon; the timeout exists so an
/// absent or wedged chip returns an error instead of hanging a kernel.
const POLL_ATTEMPTS: u32 = 1000;

/// Microseconds between poll attempts.
const POLL_DELAY_US: u32 = 10;

/// Alignment the chip requires of both ring base addresses.
///
/// It does not fault on a misaligned base — it ignores the low bits, so the
/// ring silently lands somewhere else. Checking is much cheaper than finding
/// that.
pub const RING_ALIGN: u64 = 256;

/// The driver.
pub struct Nic<R: Regs, M: Rings> {
    regs: R,
    mem: M,
    rx: RxRing,
    tx: TxRing,
    model: Model,
    mac: MacAddr,
}

impl<R: Regs, M: Rings> Nic<R, M> {
    /// Identify the chip and check the consumer's memory, refusing a part this
    /// driver has never run on.
    ///
    /// This does not touch the chip's configuration: after a failed probe the
    /// device is exactly as it was found, which matters when probing a NIC some
    /// other driver may still own.
    pub fn probe(regs: R, mem: M) -> Result<Self, Error> {
        let nic = Self::probe_unverified(regs, mem)?;
        match nic.model {
            Model::Unknown(xid) => Err(Error::UnknownChip(xid)),
            _ => Ok(nic),
        }
    }

    /// As [`Nic::probe`], but accept an unrecognised member of the family.
    ///
    /// The register block is common across the family, so this usually works.
    /// It is a separate entry point because "usually" is the caller's risk to
    /// accept explicitly rather than the driver's to take quietly.
    pub fn probe_unverified(mut regs: R, mem: M) -> Result<Self, Error> {
        check_ring(mem.rx_ring_phys(), mem.rx_ring_len())?;
        check_ring(mem.tx_ring_phys(), mem.tx_ring_len())?;

        let model = Model::from_tcr(regs.r32(regs::TCR));

        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = regs.r8(regs::IDR0 + i as u16);
        }
        let mac = MacAddr(mac);
        if !mac.is_plausible() {
            return Err(Error::ImplausibleMac(mac));
        }

        let rx = RxRing::new(mem.rx_ring_len());
        let tx = TxRing::new(mem.tx_ring_len());
        Ok(Self { regs, mem, rx, tx, model, mac })
    }

    /// The station address the chip loaded from its EEPROM.
    pub const fn mac(&self) -> MacAddr {
        self.mac
    }

    /// Which part this is.
    pub const fn model(&self) -> Model {
        self.model
    }

    /// Reset the chip and bring it up ready to pass traffic.
    pub fn init(&mut self) -> Result<(), Error> {
        self.reset()?;

        // C+ mode first — see this module's header. VLAN stripping and receive
        // checksum offload are deliberately left off: both change what the
        // buffer contains, and a driver that does not tell its caller so is
        // handing up frames that are not what arrived.
        self.regs.w8(regs::CR9346, regs::CR9346_UNLOCK);
        self.regs
            .w16(regs::CPCR, regs::CPCR_NORMAL | regs::CPCR_MULRW | regs::CPCR_TXENB);

        // Some parts come out of reset with the receiver's data-valid signal
        // gated off. Nothing arrives and nothing reports why.
        let misc = self.regs.r32(regs::MISC);
        if misc & regs::MISC_RXDV_GATED != 0 {
            self.regs.w32(regs::MISC, misc & !regs::MISC_RXDV_GATED);
        }

        self.regs.w16(regs::RMS, RX_BUF_SIZE);
        self.regs.w8(regs::MTPS, mtps_for(MAX_FRAME));

        self.populate_rings();

        // Only now may the chip be told where the rings are.
        write64(&mut self.regs, regs::RDSAR, self.mem.rx_ring_phys());
        write64(&mut self.regs, regs::TNPDS, self.mem.tx_ring_phys());
        // The high-priority ring is unused; point it nowhere rather than
        // leaving whatever the previous owner left in it.
        write64(&mut self.regs, regs::THPDS, 0);

        self.regs.w8(regs::CR, regs::CR_RE | regs::CR_TE);

        self.regs
            .w32(regs::TCR, regs::TCR_IFG_STANDARD | regs::TCR_MXDMA_UNLIMITED);
        self.regs.w32(
            regs::RCR,
            regs::RCR_APM
                | regs::RCR_AB
                | regs::RCR_AM
                | regs::RCR_MXDMA_UNLIMITED
                | regs::RCR_RXFTH_DEFAULT,
        );

        // Accept every multicast group that gets past `RCR_AM`. Filtering
        // belongs above this layer, and an all-zero hash silently drops
        // protocols that depend on multicast.
        self.regs.w32(regs::MAR0, 0xFFFF_FFFF);
        self.regs.w32(regs::MAR0 + 4, 0xFFFF_FFFF);

        self.regs.w32(regs::MPC, 0);
        self.regs.w16(regs::ISR, 0xFFFF);
        self.regs.w16(regs::IMR, regs::INT_DEFAULT_MASK);

        let cfg1 = self.regs.r8(regs::CONFIG1);
        self.regs.w8(regs::CONFIG1, cfg1 | regs::CONFIG1_DRVLOAD);
        self.regs.w8(regs::CR9346, regs::CR9346_LOCK);

        Ok(())
    }

    /// Soft-reset the chip and wait, bounded, for it to finish.
    pub fn reset(&mut self) -> Result<(), Error> {
        self.regs.w8(regs::CR, regs::CR_RST);
        for _ in 0..POLL_ATTEMPTS {
            if self.regs.r8(regs::CR) & regs::CR_RST == 0 {
                self.rx = RxRing::new(self.mem.rx_ring_len());
                self.tx = TxRing::new(self.mem.tx_ring_len());
                return Ok(());
            }
            self.regs.delay_us(POLL_DELAY_US);
        }
        Err(Error::ResetTimeout)
    }

    /// Post every receive buffer and blank the transmit ring.
    fn populate_rings(&mut self) {
        for i in 0..self.rx.len() {
            let d = self.rx.post(i, self.mem.rx_buf_phys(i), RX_BUF_SIZE);
            self.mem.set_rx_desc(i, d);
        }
        for i in 0..self.tx.len() {
            // Not owned by the chip, but still carrying end-of-ring: the marker
            // is a property of the ring's shape, not of a queued frame, and the
            // chip reads it on the pass that finds the entry it may use.
            let mut d = Desc::ZERO;
            if self.tx.is_last(i) {
                d.cmdstat = crate::desc::EOR;
            }
            self.mem.set_tx_desc(i, d);
        }
    }

    /// Queue one frame for transmission.
    ///
    /// Frames shorter than the Ethernet minimum are zero-padded rather than
    /// sent short — and padded by actually writing zeroes, not by declaring a
    /// larger length over stale buffer contents.
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), TxError> {
        if frame.is_empty() || frame.len() > MAX_FRAME {
            return Err(TxError::BadLength { len: frame.len() });
        }
        let Some(i) = self.tx.producer() else {
            return Err(TxError::Full);
        };

        let copied = self.mem.tx_buf_write(i, frame);
        let on_wire = if copied < MIN_FRAME {
            self.mem.tx_buf_zero(i, copied, MIN_FRAME);
            MIN_FRAME
        } else {
            copied
        };

        let d = self.tx.post(i, self.mem.tx_buf_phys(i), on_wire as u16);
        self.mem.set_tx_desc(i, d);
        self.tx.produce();

        // The doorbell. Without it the chip only notices the new descriptor on
        // its next pass, which on an idle link may be never.
        self.regs.w8(regs::TPPOLL, regs::TPPOLL_NPQ);
        Ok(())
    }

    /// Take one received frame, if one is waiting.
    ///
    /// Frames the chip flagged as errored, and fragments, are dropped here:
    /// their buffers are re-posted and the scan continues, so a run of bad
    /// frames does not stall the ring. Returns the number of bytes written into
    /// `dst`.
    pub fn receive(&mut self, dst: &mut [u8]) -> Option<usize> {
        for _ in 0..self.rx.len() {
            let i = self.rx.cursor();
            let d = self.mem.rx_desc(i);
            if d.owned_by_chip() {
                return None;
            }

            let len = d.rx_frame_len();
            let taken = match len {
                Some(n) => Some(self.mem.rx_buf_read(i, usize::from(n), dst)),
                None => None,
            };

            let reposted = self.rx.post(i, self.mem.rx_buf_phys(i), RX_BUF_SIZE);
            self.mem.set_rx_desc(i, reposted);
            self.rx.advance();

            if let Some(n) = taken {
                return Some(n);
            }
        }
        None
    }

    /// Hand back every transmit descriptor the chip has finished with.
    ///
    /// Returns how many were reclaimed.
    pub fn reclaim_tx(&mut self) -> usize {
        let mut n = 0;
        while let Some(i) = self.tx.consumer() {
            if self.mem.tx_desc(i).owned_by_chip() {
                break;
            }
            self.tx.consume();
            n += 1;
        }
        n
    }

    /// Whether another frame can be queued right now.
    pub const fn can_transmit(&self) -> bool {
        !self.tx.is_full()
    }

    /// Read and acknowledge the interrupt status.
    ///
    /// `ISR` is write-1-to-clear, so the bits are written straight back. Doing
    /// this in one place is what keeps an acknowledgement from clearing a bit
    /// that arrived between the read and the write: only bits actually observed
    /// are ever cleared.
    pub fn take_interrupts(&mut self) -> u16 {
        let isr = self.regs.r16(regs::ISR);
        if isr != 0 {
            self.regs.w16(regs::ISR, isr);
        }
        isr
    }

    /// Current link state, from one byte read.
    pub fn link(&mut self) -> LinkState {
        LinkState::from_phystatus(self.regs.r8(regs::PHYSTATUS))
    }

    /// Read a PHY register over MDIO.
    pub fn phy_read(&mut self, reg: u8) -> Option<u16> {
        self.regs.w32(regs::PHYAR, mdio::read_request(reg));
        for _ in 0..POLL_ATTEMPTS {
            self.regs.delay_us(POLL_DELAY_US);
            if let Some(v) = mdio::read_result(self.regs.r32(regs::PHYAR)) {
                return Some(v);
            }
        }
        None
    }

    /// Write a PHY register over MDIO. `false` on timeout.
    pub fn phy_write(&mut self, reg: u8, data: u16) -> bool {
        self.regs.w32(regs::PHYAR, mdio::write_request(reg, data));
        for _ in 0..POLL_ATTEMPTS {
            self.regs.delay_us(POLL_DELAY_US);
            if mdio::write_done(self.regs.r32(regs::PHYAR)) {
                return true;
            }
        }
        false
    }

    /// Stop the receiver and transmitter, leaving the rings intact.
    pub fn stop(&mut self) {
        self.regs.w16(regs::IMR, 0);
        self.regs.w8(regs::CR, 0);
        self.regs.w16(regs::ISR, 0xFFFF);
    }

    /// Give the register and memory interfaces back.
    pub fn release(self) -> (R, M) {
        (self.regs, self.mem)
    }
}

/// Write a 64-bit register as two words, low half first.
///
/// The order is not cosmetic on a 32-bit-wide register pair: the chip latches
/// the pair when the high word lands, so writing high-then-low can momentarily
/// present a base address made of one new half and one old one.
fn write64<R: Regs>(regs: &mut R, off: u16, val: u64) {
    regs.w32(off, val as u32);
    regs.w32(off + 4, (val >> 32) as u32);
}

/// Validate one ring's base address and length.
fn check_ring(phys: u64, len: usize) -> Result<(), Error> {
    if len == 0 || !len.is_power_of_two() {
        return Err(Error::RingLength { len });
    }
    if !phys.is_multiple_of(RING_ALIGN) {
        return Err(Error::RingMisaligned { phys });
    }
    Ok(())
}

/// `MTPS` counts 128-byte units, rounded up, and saturates at its field width.
const fn mtps_for(max_frame: usize) -> u8 {
    let units = max_frame.div_ceil(128);
    if units > 0x3F { 0x3F } else { units as u8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misaligned_ring_is_refused() {
        assert_eq!(check_ring(0x1000, 16), Ok(()));
        assert_eq!(
            check_ring(0x1080, 16),
            Err(Error::RingMisaligned { phys: 0x1080 })
        );
        // One byte short of alignment is the realistic mistake, and the chip
        // would take it silently.
        assert_eq!(
            check_ring(0x10FF, 16),
            Err(Error::RingMisaligned { phys: 0x10FF })
        );
    }

    #[test]
    fn a_ring_length_that_is_not_a_power_of_two_is_refused() {
        assert_eq!(check_ring(0x1000, 0), Err(Error::RingLength { len: 0 }));
        assert_eq!(check_ring(0x1000, 12), Err(Error::RingLength { len: 12 }));
        assert_eq!(check_ring(0x1000, 1), Ok(()));
    }

    #[test]
    fn the_transmit_size_register_covers_a_full_frame() {
        assert!(usize::from(mtps_for(MAX_FRAME)) * 128 >= MAX_FRAME);
        assert!(mtps_for(MAX_FRAME) <= 0x3F);
        // A jumbo request must saturate rather than wrap into neighbouring bits.
        assert_eq!(mtps_for(usize::MAX), 0x3F);
    }
}
