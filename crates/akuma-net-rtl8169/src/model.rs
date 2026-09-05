//! A simulated RTL8168g, good enough to run the real driver against.
//!
//! This is not a convenience mock that returns whatever the test wants. It is a
//! small state machine that behaves the way the chip behaves in the four places
//! that matter, and — the point of it — **panics when the driver breaks the
//! ownership contract**:
//!
//! * the reset bit clears itself, but only after a delay, so a driver that
//!   forgets to poll fails here rather than on hardware;
//! * `PHYAR` implements the asymmetric busy protocol from [`crate::mdio`], so
//!   polling for the wrong edge hangs the test instead of silently reading
//!   zero;
//! * writing the transmit doorbell walks the ring exactly as the chip does,
//!   takes only descriptors it owns, and puts the bytes on [`Wire`];
//! * [`FakeChip::deliver`] pushes a frame into the receive ring the same way.
//!
//! Touching a descriptor or buffer the chip owns is a panic, not a wrong
//! answer. On real silicon that mistake is a data race with a DMA engine: it
//! corrupts a frame, or a neighbouring allocation, intermittently, under load.
//! Here it is a failing test with a line number.
//!
//! # Why one object serves both traits
//!
//! [`Nic`](crate::Nic) takes a register interface and a memory interface
//! separately, because on real hardware they are separate things — a BAR
//! mapping and a DMA allocation. A chip is one object, so [`FakeChip`] hands
//! out [`Port`]s: cheap handles that share its state through a `RefCell`. Two
//! of them go into the driver, and the test keeps the `FakeChip` itself to
//! inject frames and inspect the wire.

use core::cell::RefCell;

use crate::desc::{self, Desc};
use crate::{MacAddr, Regs, Rings, regs};

/// Receive ring entries in the model.
pub const RX_LEN: usize = 8;
/// Transmit ring entries in the model.
pub const TX_LEN: usize = 8;
/// Bytes per frame buffer.
pub const BUF_LEN: usize = 2048;
/// Frames [`Wire`] remembers.
pub const WIRE_LEN: usize = 16;
/// Register writes [`WriteLog`] remembers.
pub const LOG_LEN: usize = 128;

/// Device address the model reports for its receive ring.
pub const RX_RING_PHYS: u64 = 0x0000_0001_0000_0000;
/// Device address the model reports for its transmit ring.
pub const TX_RING_PHYS: u64 = 0x0000_0001_0000_1000;
/// Device address of receive buffer 0; buffers are [`BUF_LEN`] apart.
pub const RX_BUF_BASE: u64 = 0x0000_0001_0010_0000;
/// Device address of transmit buffer 0.
pub const TX_BUF_BASE: u64 = 0x0000_0001_0020_0000;

/// The station address the model's EEPROM "loaded".
pub const MODEL_MAC: MacAddr = MacAddr([0x60, 0x02, 0x92, 0x61, 0x4e, 0x73]);

/// `TCR` revision bits of an RTL8168g — what the reference chip reports.
pub const MODEL_TCR_HWREV: u32 = 0x4c00_0000;

/// One transmitted frame.
#[derive(Clone, Copy)]
pub struct Frame {
    /// Frame bytes, of which the first [`Frame::len`] are meaningful.
    pub bytes: [u8; BUF_LEN],
    /// Length as the chip was told to send it.
    pub len: usize,
}

impl Frame {
    /// The meaningful bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Frames the model has transmitted, oldest first.
pub struct Wire {
    frames: [Frame; WIRE_LEN],
    count: usize,
}

impl Wire {
    /// How many frames have been sent.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }
    /// Whether nothing has been sent.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Frame `i`, oldest first.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&Frame> {
        (i < self.count).then(|| &self.frames[i])
    }
    /// The most recently sent frame.
    #[must_use]
    pub fn last(&self) -> Option<&Frame> {
        self.count.checked_sub(1).map(|i| &self.frames[i])
    }
}

/// One recorded register write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoggedWrite {
    /// Register offset.
    pub off: u16,
    /// Access width in bytes: 1, 2 or 4.
    pub width: u8,
    /// Value written.
    pub val: u32,
}

/// Every register write the driver made, in order.
///
/// Init has ordering constraints that no single register value can express —
/// "C+ mode before the ring bases" is a fact about sequence. This is how a test
/// asserts one.
pub struct WriteLog {
    entries: [LoggedWrite; LOG_LEN],
    count: usize,
}

impl WriteLog {
    /// Number of writes recorded (writes past [`LOG_LEN`] are dropped).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }
    /// Whether nothing was recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Position of the first write to `off`, if any.
    #[must_use]
    pub fn first_write_to(&self, off: u16) -> Option<usize> {
        self.entries[..self.count].iter().position(|e| e.off == off)
    }
    /// Position of the last write to `off`, if any.
    #[must_use]
    pub fn last_write_to(&self, off: u16) -> Option<usize> {
        self.entries[..self.count].iter().rposition(|e| e.off == off)
    }
    /// The value of the last write to `off`, if any.
    #[must_use]
    pub fn last_value(&self, off: u16) -> Option<u32> {
        self.last_write_to(off).map(|i| self.entries[i].val)
    }
    /// Whether `a` was first written before `b` was.
    #[must_use]
    pub fn wrote_before(&self, a: u16, b: u16) -> bool {
        match (self.first_write_to(a), self.first_write_to(b)) {
            (Some(x), Some(y)) => x < y,
            _ => false,
        }
    }
}

struct State {
    regs: [u8; 256],
    phy: [u16; 32],
    rx_desc: [Desc; RX_LEN],
    tx_desc: [Desc; TX_LEN],
    rx_buf: [[u8; BUF_LEN]; RX_LEN],
    tx_buf: [[u8; BUF_LEN]; TX_LEN],
    /// Where the chip will look next for a frame to send.
    tx_cursor: usize,
    /// Where the chip will put the next arriving frame.
    rx_cursor: usize,
    /// Polls remaining before the reset bit clears.
    reset_countdown: u8,
    wire: Wire,
    log: WriteLog,
}

/// A simulated chip.
pub struct FakeChip {
    state: RefCell<State>,
}

impl Default for FakeChip {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeChip {
    /// A chip as found at power-on: MAC loaded, revision readable, idle.
    ///
    /// The frame buffers make this a large value — two rings' worth of 2 KiB
    /// buffers, plus the wire. That is deliberate: a model whose buffers cannot
    /// hold a full-size Ethernet frame would silently stop testing the case
    /// that matters most. Tests hold one of these, once.
    #[must_use]
    #[allow(clippy::large_stack_arrays, reason = "frame buffers must be frame-sized")]
    pub fn new() -> Self {
        let mut regs = [0u8; 256];
        regs[..6].copy_from_slice(&MODEL_MAC.0);
        write_reg32(&mut regs, super::regs::TCR, MODEL_TCR_HWREV);

        let mut phy = [0u16; 32];
        phy[usize::from(crate::mdio::REG_BMSR)] = crate::mdio::BMSR_LINK;

        Self {
            state: RefCell::new(State {
                regs,
                phy,
                rx_desc: [Desc::ZERO; RX_LEN],
                tx_desc: [Desc::ZERO; TX_LEN],
                rx_buf: [[0; BUF_LEN]; RX_LEN],
                tx_buf: [[0; BUF_LEN]; TX_LEN],
                tx_cursor: 0,
                rx_cursor: 0,
                reset_countdown: 0,
                wire: Wire { frames: [Frame { bytes: [0; BUF_LEN], len: 0 }; WIRE_LEN], count: 0 },
                log: WriteLog {
                    entries: [LoggedWrite { off: 0, width: 0, val: 0 }; LOG_LEN],
                    count: 0,
                },
            }),
        }
    }

    /// A handle implementing both [`Regs`] and [`Rings`].
    ///
    /// Take two: the driver wants a register interface and a memory interface,
    /// and on a real machine those are two different objects.
    pub fn port(&self) -> Port<'_> {
        Port { state: &self.state }
    }

    /// Set the byte the driver reads as link status.
    pub fn set_phystatus(&self, raw: u8) {
        self.state.borrow_mut().regs[usize::from(regs::PHYSTATUS)] = raw;
    }

    /// Set a PHY register the driver can reach over MDIO.
    pub fn set_phy(&self, reg: u8, val: u16) {
        self.state.borrow_mut().phy[usize::from(reg) & 0x1F] = val;
    }

    /// Raise interrupt status bits, as the chip would.
    pub fn raise_interrupt(&self, bits: u16) {
        let mut s = self.state.borrow_mut();
        let cur = read_reg16(&s.regs, regs::ISR);
        write_reg16(&mut s.regs, regs::ISR, cur | bits);
    }

    /// Whether the driver has enabled the receiver and transmitter.
    pub fn running(&self) -> bool {
        let cr = self.state.borrow().regs[usize::from(regs::CR)];
        cr & (regs::CR_RE | regs::CR_TE) == (regs::CR_RE | regs::CR_TE)
    }

    /// Read a register directly, without going through the driver.
    pub fn peek32(&self, off: u16) -> u32 {
        read_reg32(&self.state.borrow().regs, off)
    }

    /// As [`FakeChip::peek32`], one halfword.
    pub fn peek16(&self, off: u16) -> u16 {
        read_reg16(&self.state.borrow().regs, off)
    }

    /// As [`FakeChip::peek32`], one byte.
    pub fn peek8(&self, off: u16) -> u8 {
        self.state.borrow().regs[usize::from(off)]
    }

    /// Deliver a frame to the driver, as the wire would.
    ///
    /// Returns `false` when the receive ring has no entry the chip owns — which
    /// is exactly the overflow the real chip reports as a missed packet, not an
    /// error in the test.
    pub fn deliver(&self, frame: &[u8]) -> bool {
        self.deliver_raw(frame, desc::FS | desc::LS, 0)
    }

    /// Deliver a frame with chosen status bits — for testing the error paths.
    ///
    /// `extra_status` is OR-ed into the descriptor (e.g. [`desc::RX_RES`]);
    /// `len_override`, when non-zero, replaces the length the chip reports,
    /// so a test can produce a frame whose stated length disagrees with what
    /// was written.
    pub fn deliver_raw(&self, frame: &[u8], flags: u32, len_override: u16) -> bool {
        let mut s = self.state.borrow_mut();
        let i = s.rx_cursor;
        if !s.rx_desc[i].owned_by_chip() {
            return false;
        }

        let n = frame.len().min(BUF_LEN);
        s.rx_buf[i][..n].copy_from_slice(&frame[..n]);

        // The chip reports the length with the frame check sequence included;
        // the driver is the thing that has to know to subtract it.
        let reported = if len_override != 0 {
            u32::from(len_override)
        } else {
            (n as u32) + u32::from(desc::FCS_LEN)
        };

        let eor = s.rx_desc[i].cmdstat & desc::EOR;
        s.rx_desc[i].cmdstat = eor | flags | (reported & desc::RX_LEN_MASK);
        s.rx_cursor = (i + 1) % RX_LEN;

        let isr = read_reg16(&s.regs, regs::ISR);
        write_reg16(&mut s.regs, regs::ISR, isr | regs::INT_ROK);
        true
    }

    /// Frames the model has transmitted.
    pub fn wire(&self) -> core::cell::Ref<'_, Wire> {
        core::cell::Ref::map(self.state.borrow(), |s| &s.wire)
    }

    /// Every register write the driver made.
    pub fn log(&self) -> core::cell::Ref<'_, WriteLog> {
        core::cell::Ref::map(self.state.borrow(), |s| &s.log)
    }

    /// Descriptor `i` of the receive ring, as the chip sees it.
    pub fn rx_desc(&self, i: usize) -> Desc {
        self.state.borrow().rx_desc[i]
    }

    /// Descriptor `i` of the transmit ring, as the chip sees it.
    pub fn tx_desc(&self, i: usize) -> Desc {
        self.state.borrow().tx_desc[i]
    }
}

/// A handle onto a [`FakeChip`], implementing [`Regs`] and [`Rings`].
pub struct Port<'a> {
    state: &'a RefCell<State>,
}

impl State {
    /// Everything the chip does when the transmit doorbell is rung.
    fn run_transmit(&mut self) {
        // Bounded by the ring length: one doorbell drains at most one lap, and
        // an unbounded loop here would hide a driver that never clears OWN.
        for _ in 0..TX_LEN {
            let i = self.tx_cursor;
            let d = self.tx_desc[i];
            if !d.owned_by_chip() {
                break;
            }
            let len = (d.cmdstat & desc::TX_LEN_MASK) as usize;
            let len = len.min(BUF_LEN);

            if self.wire.count < WIRE_LEN {
                let slot = self.wire.count;
                self.wire.frames[slot].bytes[..len].copy_from_slice(&self.tx_buf[i][..len]);
                self.wire.frames[slot].len = len;
                self.wire.count += 1;
            }

            // Hand the descriptor back, preserving the ring marker.
            self.tx_desc[i].cmdstat = d.cmdstat & desc::EOR;
            self.tx_cursor = (i + 1) % TX_LEN;

            let isr = read_reg16(&self.regs, regs::ISR);
            write_reg16(&mut self.regs, regs::ISR, isr | regs::INT_TOK);
        }
    }

    /// The `PHYAR` request/response port.
    fn phyar_write(&mut self, val: u32) {
        let reg = ((val & crate::mdio::REG_MASK) >> crate::mdio::REG_SHIFT) as usize;
        let answer = if val & crate::mdio::BUSY != 0 {
            // A write: take the data, then clear BUSY to report completion.
            self.phy[reg & 0x1F] = (val & crate::mdio::DATA_MASK) as u16;
            val & !crate::mdio::BUSY
        } else {
            // A read: set BUSY to report the answer is present.
            crate::mdio::BUSY | u32::from(self.phy[reg & 0x1F])
        };
        write_reg32(&mut self.regs, regs::PHYAR, answer);
    }
}

impl Regs for Port<'_> {
    fn r8(&mut self, off: u16) -> u8 {
        let mut s = self.state.borrow_mut();
        if off == regs::CR && s.reset_countdown > 0 {
            s.reset_countdown -= 1;
            if s.reset_countdown == 0 {
                let cr = s.regs[usize::from(regs::CR)];
                s.regs[usize::from(regs::CR)] = cr & !regs::CR_RST;
            }
        }
        s.regs[usize::from(off)]
    }

    fn r16(&mut self, off: u16) -> u16 {
        read_reg16(&self.state.borrow().regs, off)
    }

    fn r32(&mut self, off: u16) -> u32 {
        read_reg32(&self.state.borrow().regs, off)
    }

    fn w8(&mut self, off: u16, val: u8) {
        let mut s = self.state.borrow_mut();
        s.log.record(off, 1, u32::from(val));
        s.regs[usize::from(off)] = val;

        if off == regs::CR && val & regs::CR_RST != 0 {
            // A real reset takes time. Clearing it on the first read would let
            // a driver that never polls pass.
            s.reset_countdown = 3;
            s.tx_cursor = 0;
            s.rx_cursor = 0;
        }
        if off == regs::TPPOLL && val & regs::TPPOLL_NPQ != 0 {
            s.run_transmit();
        }
    }

    fn w16(&mut self, off: u16, val: u16) {
        let mut s = self.state.borrow_mut();
        s.log.record(off, 2, u32::from(val));
        if off == regs::ISR {
            // Write-1-to-clear, like the real register. A model that stored the
            // value would let a driver "acknowledge" by writing zero.
            let cur = read_reg16(&s.regs, regs::ISR);
            write_reg16(&mut s.regs, regs::ISR, cur & !val);
            return;
        }
        write_reg16(&mut s.regs, off, val);
    }

    fn w32(&mut self, off: u16, val: u32) {
        let mut s = self.state.borrow_mut();
        s.log.record(off, 4, val);
        if off == regs::PHYAR {
            s.phyar_write(val);
            return;
        }
        write_reg32(&mut s.regs, off, val);
    }

    fn delay_us(&mut self, _us: u32) {}
}

impl WriteLog {
    fn record(&mut self, off: u16, width: u8, val: u32) {
        if self.count < LOG_LEN {
            self.entries[self.count] = LoggedWrite { off, width, val };
            self.count += 1;
        }
    }
}

impl Rings for Port<'_> {
    fn rx_ring_len(&self) -> usize {
        RX_LEN
    }
    fn tx_ring_len(&self) -> usize {
        TX_LEN
    }
    fn rx_ring_phys(&self) -> u64 {
        RX_RING_PHYS
    }
    fn tx_ring_phys(&self) -> u64 {
        TX_RING_PHYS
    }

    fn rx_desc(&self, i: usize) -> Desc {
        self.state.borrow().rx_desc[i]
    }

    fn set_rx_desc(&mut self, i: usize, d: Desc) {
        let mut s = self.state.borrow_mut();
        assert!(
            !s.rx_desc[i].owned_by_chip(),
            "driver wrote receive descriptor {i} while the chip owned it — \
             on real hardware this races the DMA engine"
        );
        s.rx_desc[i] = d;
    }

    fn tx_desc(&self, i: usize) -> Desc {
        self.state.borrow().tx_desc[i]
    }

    fn set_tx_desc(&mut self, i: usize, d: Desc) {
        let mut s = self.state.borrow_mut();
        assert!(
            !s.tx_desc[i].owned_by_chip(),
            "driver wrote transmit descriptor {i} while the chip owned it — \
             the frame already queued there would be replaced mid-DMA"
        );
        s.tx_desc[i] = d;
    }

    fn rx_buf_phys(&self, i: usize) -> u64 {
        RX_BUF_BASE + (i as u64) * BUF_LEN as u64
    }

    fn tx_buf_phys(&self, i: usize) -> u64 {
        TX_BUF_BASE + (i as u64) * BUF_LEN as u64
    }

    fn rx_buf_read(&self, i: usize, len: usize, dst: &mut [u8]) -> usize {
        let s = self.state.borrow();
        assert!(
            !s.rx_desc[i].owned_by_chip(),
            "driver read receive buffer {i} while the chip owned it — \
             the frame may be half-written"
        );
        let n = len.min(dst.len()).min(BUF_LEN);
        dst[..n].copy_from_slice(&s.rx_buf[i][..n]);
        n
    }

    fn tx_buf_write(&mut self, i: usize, src: &[u8]) -> usize {
        let mut s = self.state.borrow_mut();
        assert!(
            !s.tx_desc[i].owned_by_chip(),
            "driver wrote transmit buffer {i} while the chip owned it — \
             this corrupts a frame already being sent"
        );
        let n = src.len().min(BUF_LEN);
        s.tx_buf[i][..n].copy_from_slice(&src[..n]);
        n
    }

    fn tx_buf_zero(&mut self, i: usize, from: usize, to: usize) {
        let mut s = self.state.borrow_mut();
        assert!(
            !s.tx_desc[i].owned_by_chip(),
            "driver zeroed transmit buffer {i} while the chip owned it"
        );
        let (from, to) = (from.min(BUF_LEN), to.min(BUF_LEN));
        if from < to {
            s.tx_buf[i][from..to].fill(0);
        }
    }
}

// --- little-endian register file helpers ------------------------------------

fn read_reg16(regs: &[u8; 256], off: u16) -> u16 {
    let o = usize::from(off);
    u16::from_le_bytes([regs[o], regs[o + 1]])
}

fn read_reg32(regs: &[u8; 256], off: u16) -> u32 {
    let o = usize::from(off);
    u32::from_le_bytes([regs[o], regs[o + 1], regs[o + 2], regs[o + 3]])
}

fn write_reg16(regs: &mut [u8; 256], off: u16, val: u16) {
    let o = usize::from(off);
    regs[o..o + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_reg32(regs: &mut [u8; 256], off: u16, val: u32) {
    let o = usize::from(off);
    regs[o..o + 4].copy_from_slice(&val.to_le_bytes());
}
