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
//! 2. **Descriptors before the base addresses, base addresses and the filter
//!    and transmit configuration before [`regs::CR_RE`].** The moment the
//!    receiver is enabled the chip may DMA into whatever the base points at,
//!    and a frame that arrives under a half-programmed `RCR` is accepted with
//!    whatever the reset defaults were — a real case across a warm reset with
//!    live link. The datapath enable is therefore the *last* write `init`
//!    makes.
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

/// Microseconds [`Nic::kick_receiver`] waits between dropping `CR_RE` and
/// restating `RDSAR`.
///
/// This family has no receiver-idle bit to poll, so the drain of whatever DMA
/// is in flight cannot be observed — only waited out. The window it covers is
/// a descriptor completion or two, microseconds at most; the delay is bounded
/// and short next to the multi-second silences that reach this path.
const RE_QUIESCE_US: u32 = 20;

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
        // The high-priority ring is unused — nothing in this driver ever
        // writes `TPPOLL_HPQ` — but it is aliased onto the normal ring rather
        // than pointed at physical zero. Zero is a real address: a chip that
        // ever scanned it would walk descriptors out of the bottom of memory,
        // DMAing over whatever lives there, with no error bit set anywhere.
        write64(&mut self.regs, regs::THPDS, self.mem.tx_ring_phys());

        // Filter, burst and transmit configuration land *before* the
        // datapaths come on: a frame that arrives under a half-programmed
        // `RCR` is accepted with the reset defaults, and a warm reset with a
        // live link is a real case. (`the_receiver_is_enabled_only_after…`
        // in `tests/bringup.rs` pins the order.)
        self.regs
            .w32(regs::TCR, regs::TCR_IFG_STANDARD | regs::TCR_MXDMA_UNLIMITED);
        self.regs.w32(regs::RCR, RCR_VALUE);

        // Accept every multicast group that gets past `RCR_AM`. Filtering
        // belongs above this layer, and an all-zero hash silently drops
        // protocols that depend on multicast.
        self.regs.w32(regs::MAR0, 0xFFFF_FFFF);
        self.regs.w32(regs::MAR0 + 4, 0xFFFF_FFFF);

        self.regs.w32(regs::MPC, 0);

        // Acknowledge everything left over, then keep the chip quiet: this
        // driver owns no interrupt line, and a raised line nobody serves is
        // worse than a quiet one. A consumer that has routed the line arms
        // its own mask via [`Nic::arm_interrupts`] after this returns —
        // [`regs::INT_DEFAULT_MASK`] is the set to start from.
        self.regs.w16(regs::ISR, 0xFFFF);
        self.regs.w16(regs::IMR, 0);

        let cfg1 = self.regs.r8(regs::CONFIG1);
        self.regs.w8(regs::CONFIG1, cfg1 | regs::CONFIG1_DRVLOAD);
        self.regs.w8(regs::CR9346, regs::CR9346_LOCK);

        // Last of all, the datapaths: everything they touch — rings,
        // descriptors, buffers, filter, transmit configuration — is in place.
        self.regs.w8(regs::CR, regs::CR_RE | regs::CR_TE);

        Ok(())
    }

    /// Hand the interrupt mask to the chip.
    ///
    /// [`Nic::init`] leaves it clear — this crate is polled, and arming
    /// sources with no handler on the line is how a box that cannot service
    /// interrupts gets an unacknowledged level-triggered line. A consumer
    /// that has routed the interrupt and registered a handler calls this
    /// once, with exactly the sources it will service;
    /// [`regs::INT_DEFAULT_MASK`] is the set a working Linux driver leaves
    /// on the reference chip and is a reasonable starting point.
    pub fn arm_interrupts(&mut self, mask: u16) {
        self.regs.w16(regs::IMR, mask);
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

        // A frame longer than the buffer behind its descriptor is refused,
        // not truncated. `tx_buf_write` clamps its copy to what fits, and a
        // clamped frame would be acknowledged as sent while its tail never
        // left — the one failure mode worse than a drop. The reference glue
        // sizes buffers above `MAX_FRAME`, so this guards the consumer with
        // tighter memory (see the `Rings` contract).
        if frame.len() > self.mem.tx_buf_len(i) {
            return Err(TxError::BadLength { len: frame.len() });
        }

        let copied = self.mem.tx_buf_write(i, frame);
        debug_assert_eq!(copied, frame.len(), "tx_buf_write truncated a checked frame");
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
            // A frame that does not fit `dst` is refused rather than
            // truncated. `rx_buf_read` clamps its copy to what fits, and a
            // clamped frame is indistinguishable from a valid short one once
            // it leaves this function — delivering it poisons everything
            // above with no indication anything was lost. Like an errored
            // frame it is re-posted below and the scan continues.
            let taken = match len {
                Some(n) if usize::from(n) <= dst.len() => {
                    Some(self.mem.rx_buf_read(i, usize::from(n), dst))
                }
                _ => None,
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

    /// Which receive descriptor the driver will inspect next.
    ///
    /// Exposed because the cursor's disagreement with the chip *is* the fault
    /// this driver has to detect; a number the tests cannot read is a number
    /// the tests cannot assert about.
    #[must_use]
    pub const fn rx_cursor(&self) -> usize {
        self.rx.cursor()
    }

    /// Move the receive cursor to the oldest descriptor the chip has
    /// completed, **without disturbing a single descriptor**. Returns the
    /// index if the cursor moved.
    ///
    /// [`Self::receive`] gives up the moment the descriptor under the cursor
    /// is chip-owned, which is correct while the two agree and is a permanent
    /// stall once they do not: measured on the HP box 2026-09-20, the cursor
    /// sat at 15 while a complete 154-byte frame waited at slot 0, and every
    /// lap for the rest of the boot asked slot 15 the same question.
    ///
    /// Why a scan is safe when advancing is not. `advance` is a *consumption* —
    /// it re-posts the descriptor it leaves, so walking it past a chip-owned
    /// slot would hand the chip back a buffer it is about to write into and
    /// break lockstep in the other direction. This reads and moves nothing but
    /// the cursor, and the first not-chip-owned slot at or after an owned
    /// cursor is the oldest frame the chip has finished, in **both** desync
    /// directions:
    ///
    /// * the cursor slot is complete — the cursor is honest, nothing needs
    ///   repairing, and `None` says so (`receive` will find the frame exactly
    ///   where it is);
    /// * the cursor slot is owned and a later slot is not — the chip's fill
    ///   position is *behind* the cursor (its receiver restarted and resumed
    ///   from the ring base), and the seek lands on the oldest completed
    ///   frame.
    ///
    /// The scan therefore starts **at the cursor, not past it**. An earlier
    /// revision started at cursor+1, which repaired the rewind case but let a
    /// burst that completed the cursor slot and the one behind it between
    /// laps steer the seek past the oldest frame — out-of-order delivery, and
    /// a frame stranded behind the cursor until a full ring wrap. Reproduced
    /// in `tests/resync_reorder_repro.rs`; see
    /// `docs/archive/NETWORKING_AUDIT.md` §2 for the failure analysis.
    ///
    /// Cheaper than [`Self::kick_receiver`], and strictly better where it
    /// works: no MMIO, no stopping the receiver, and nothing in flight is lost
    /// across it. It is not a substitute for the kick when the chip's receiver
    /// is genuinely wedged — it only repairs the *disagreement*.
    pub fn resync_rx_cursor(&mut self) -> Option<usize> {
        let len = self.rx.len();
        let start = self.rx.cursor();
        for step in 0..len {
            let i = (start + step) & (len - 1);
            if !self.mem.rx_desc(i).owned_by_chip() {
                if step == 0 {
                    // The frame under the cursor is exactly where `receive`
                    // looks next. Nothing to repair — and `None`, so this lap
                    // does not count as one in the caller's tallies.
                    return None;
                }
                self.rx.seek(i);
                return Some(i);
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

    /// Try to restart a receiver that has gone quiet with nothing to say.
    ///
    /// Called when receive has stopped while every visible condition is
    /// correct: ring base right, all descriptors owned by the chip with
    /// end-of-ring where it belongs, `CR_RE` set, `ISR` clear, and — the
    /// telling one — `MPC` at zero, meaning the chip is not even discarding
    /// frames for want of a descriptor. It is not overrun and it is not
    /// confused about the ring; it is not being handed frames at all.
    ///
    /// The one gate between the PHY and the MAC that can do that silently is
    /// [`regs::MISC_RXDV_GATED`], which `init` already clears once. Some
    /// RTL8168 parts re-assert it later, and nothing reports that they have.
    ///
    /// This clears it again, waits the receiver's in-flight DMA out, re-writes
    /// the ring base and the accept configuration, and bounces `CR_RE`.
    /// Returns `MISC` before and after, so a caller can say whether the gate
    /// was actually the thing found set — which is the difference between a
    /// fix and a coincidence.
    ///
    /// What happens to frames in flight is worth stating exactly. A frame the
    /// chip completed before the restart stays `OWN=0` in its slot: the
    /// restarted walk takes only owned descriptors and skips it, and the
    /// cursor reset below delivers it *late* — out of ring order, when the
    /// cursor reaches the slot again. The kick does not lose that frame; it
    /// misorders it. (An earlier comment here said "loses", which understated
    /// the damage differently — see `docs/archive/NETWORKING_AUDIT.md` §6.)
    pub fn kick_receiver(&mut self) -> (u32, u32) {
        let before = self.regs.r32(regs::MISC);
        if before & regs::MISC_RXDV_GATED != 0 {
            self.regs.w32(regs::MISC, before & !regs::MISC_RXDV_GATED);
        }

        // Drop RE while the ring base is re-stated: the chip latches RDSAR when
        // the receiver starts, and writing it under a running receiver is what
        // the datasheet tells you not to do. There is no receiver-idle bit to
        // poll on this family, so the in-flight DMA is waited out — bounded,
        // and short next to the fault that got us here (`RE_QUIESCE_US`).
        let cr = self.regs.r8(regs::CR);
        self.regs.w8(regs::CR, cr & !regs::CR_RE);
        self.regs.delay_us(RE_QUIESCE_US);
        write64(&mut self.regs, regs::RDSAR, self.mem.rx_ring_phys());
        self.regs.w32(regs::RCR, RCR_VALUE);
        self.regs.w8(regs::CR, cr | regs::CR_RE);

        // The driver's own cursor goes back to the start with it: after a
        // restart the chip fills from the ring base, and a cursor left mid-ring
        // would look past the entries it is about to write.
        self.rx = RxRing::new(self.mem.rx_ring_len());

        (before, self.regs.r32(regs::MISC))
    }

    /// Everything the chip will tell us about its own receive state.
    ///
    /// Built because the alternative was inference. On the HP box the receiver
    /// took exactly `RING_LEN` frames into the ring it was given — correctly,
    /// at the addresses it was told — and then began writing completions into
    /// kernel memory 182 KiB *below* the ring, with no error bit set anywhere.
    /// Two rounds of reasoning about descriptor handling produced two wrong
    /// answers (`RDU`, then a mis-translated address), because the one thing
    /// nobody had done was ask the chip where it thought its ring was.
    ///
    /// `mpc` is the quiet one: the missed-packet counter increments for every
    /// frame dropped because no descriptor was available, and it is the
    /// difference between "the wire went silent" and "we stopped keeping up".
    #[must_use]
    pub fn snapshot(&mut self) -> Snapshot {
        Snapshot {
            cr: self.regs.r8(regs::CR),
            isr: self.regs.r16(regs::ISR),
            imr: self.regs.r16(regs::IMR),
            rcr: self.regs.r32(regs::RCR),
            tcr: self.regs.r32(regs::TCR),
            mpc: self.regs.r32(regs::MPC),
            misc: self.regs.r32(regs::MISC),
            rdsar: read64(&mut self.regs, regs::RDSAR),
            tnpds: read64(&mut self.regs, regs::TNPDS),
            cursor: self.rx.cursor(),
            ring_len: self.rx.len(),
        }
    }

    /// Receive descriptor `i`, for a caller dumping the ring.
    #[must_use]
    pub fn rx_desc_at(&self, i: usize) -> Desc {
        self.mem.rx_desc(i)
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

    /// Re-ring the transmit doorbell without queueing anything.
    ///
    /// The first-line recovery for a transmitter that has stopped draining
    /// the ring: the doorbell is what tells the chip to scan the ring again,
    /// and a wedged transmitter has in some states merely stopped looking.
    /// The transmit stall watch (`stall::tx_decide`'s consumer) escalates to
    /// a full [`Nic::init`] when re-ringing does not bring completions back.
    pub fn ring_transmitter(&mut self) {
        self.regs.w8(regs::TPPOLL, regs::TPPOLL_NPQ);
    }

    /// Stop the receiver and transmitter, leaving the rings intact.
    ///
    /// This does **not** wait for DMA to quiesce: the chip may still own
    /// descriptors for a few bus cycles after `CR` clears, and this family
    /// has no status bit that reports the drain. A caller about to unmap or
    /// reuse the ring memory owns that wait.
    pub fn stop(&mut self) {
        self.regs.w16(regs::IMR, 0);
        self.regs.w8(regs::CR, 0);
        self.regs.w16(regs::ISR, 0xFFFF);
    }

    /// Give the register and memory interfaces back.
    ///
    /// As with [`Nic::stop`], DMA quiescence is the caller's obligation: if
    /// the chip was not stopped first, it may still write into the rings
    /// this returns.
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

/// The receive configuration, in one place so `init` and [`Nic::kick_receiver`]
/// cannot drift apart.
const RCR_VALUE: u32 = regs::RCR_APM
    | regs::RCR_AB
    | regs::RCR_AM
    | regs::RCR_MXDMA_UNLIMITED
    | regs::RCR_RXFTH_DEFAULT;

fn read64<R: Regs>(regs: &mut R, off: u16) -> u64 {
    u64::from(regs.r32(off)) | (u64::from(regs.r32(off + 4)) << 32)
}

/// What the chip says about itself. See [`Nic::snapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Command register: `CR_RE`/`CR_TE` say whether receive and transmit are
    /// even enabled. A cleared `RE` is a receiver that has been switched off.
    pub cr: u8,
    /// Interrupt status, as latched right now.
    pub isr: u16,
    /// Interrupt mask.
    pub imr: u16,
    /// Receive configuration: the accept bits and the DMA burst size.
    pub rcr: u32,
    /// Transmit configuration.
    pub tcr: u32,
    /// Missed packets — frames dropped for want of a descriptor.
    pub mpc: u32,
    /// The MISC register, for [`regs::MISC_RXDV_GATED`].
    pub misc: u32,
    /// **Where the chip believes its receive ring is.** The number this whole
    /// structure exists for.
    pub rdsar: u64,
    /// Where it believes the normal-priority transmit ring is.
    pub tnpds: u64,
    /// The driver's own cursor into the ring.
    pub cursor: usize,
    /// Entries in the ring.
    pub ring_len: usize,
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
