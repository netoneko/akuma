//! A pair of Intel 8259A interrupt controllers, master and cascaded slave.
//!
//! With no local APIC and no I/O APIC (the guest is told `noapic nolapic`), the
//! legacy PICs are the whole interrupt fabric: IRQ0 is the PIT, IRQ2 is the
//! slave's INT output. The model is the datasheet's, driven through the same
//! four ports a PC has (master `0x20/0x21`, slave `0xA0/0xA1`):
//!
//! * the ICW1-ICW4 initialisation sequence (vector base, cascade, auto-EOI);
//! * OCW1 (the interrupt mask), OCW2 (non-specific and specific EOI) and OCW3
//!   (read IRR or ISR);
//! * edge-triggered inputs, fixed priority (IRQ0 highest), fully nested mode.
//!
//! Not modelled: level-triggered mode (ELCR), rotating priorities, special mask
//! mode and the poll command — the commands are accepted and have no effect,
//! which is what a guest that never uses them cannot tell from the real chip.
//!
//! The device delivers nothing itself. The hypervisor asks [`PicPair::pending`]
//! for the vector to offer the CPU, and calls [`PicPair::acknowledge`] when the
//! CPU has taken it (the INTA cycle).

#[derive(Clone, Copy, Debug)]
enum Init {
    Idle,
    /// Expecting ICW2 (vector base).
    Icw2,
    /// Expecting ICW3 (cascade wiring).
    Icw3,
    /// Expecting ICW4.
    Icw4,
}

/// One 8259A; each bool is one flip-flop of the chip.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct Pic {
    irr: u8,
    isr: u8,
    imr: u8,
    base: u8,
    init: Init,
    /// ICW1 bit 1 clear: cascade mode, ICW3 follows.
    cascade: bool,
    /// ICW1 bit 0: ICW4 follows.
    need_icw4: bool,
    auto_eoi: bool,
    /// OCW3: the command port reads ISR rather than IRR.
    read_isr: bool,
}

impl Pic {
    const fn new() -> Self {
        Self {
            irr: 0,
            isr: 0,
            imr: 0xFF,
            base: 0,
            init: Init::Idle,
            cascade: true,
            need_icw4: false,
            auto_eoi: false,
            read_isr: false,
        }
    }

    /// The line that would interrupt now, given the request lines `irr` — own
    /// IRR plus, for the master, the slave's INT on IR2. A request needs to be
    /// unmasked and strictly higher in priority than everything in service.
    const fn highest(&self, irr: u8) -> Option<u8> {
        let req = irr & !self.imr;
        let mut line = 0u8;
        while line < 8 {
            let bit = 1u8 << line;
            // Anything in service at this priority or higher blocks the rest.
            if self.isr & bit != 0 {
                return None;
            }
            if req & bit != 0 {
                return Some(line);
            }
            line += 1;
        }
        None
    }

    fn command_write(&mut self, v: u8) {
        if v & 0x10 != 0 {
            // ICW1: starts initialisation. The mask, requests and service
            // state are cleared, and priority is reset.
            self.irr = 0;
            self.isr = 0;
            self.imr = 0;
            self.auto_eoi = false;
            self.read_isr = false;
            self.cascade = v & 0x02 == 0;
            self.need_icw4 = v & 0x01 != 0;
            self.init = Init::Icw2;
        } else if v & 0x08 != 0 {
            // OCW3. Bits 1:0 = 1x select the register the command port reads.
            if v & 0x02 != 0 {
                self.read_isr = v & 0x01 != 0;
            }
        } else {
            // OCW2: bits 7:5 are the command, 2:0 a level for the specific ones.
            let level = v & 7;
            match v >> 5 {
                // Non-specific EOI (also: rotate on non-specific EOI): the
                // highest-priority line in service.
                0b001 | 0b101 => {
                    if self.isr != 0 {
                        self.isr &= self.isr - 1;
                    }
                }
                // Specific EOI (also: rotate on specific EOI).
                0b011 | 0b111 => self.isr &= !(1 << level),
                _ => {}
            }
        }
    }

    fn data_write(&mut self, v: u8) {
        match self.init {
            Init::Idle => self.imr = v,
            Init::Icw2 => {
                self.base = v & 0xF8;
                self.init = if self.cascade {
                    Init::Icw3
                } else if self.need_icw4 {
                    Init::Icw4
                } else {
                    Init::Idle
                };
            }
            Init::Icw3 => {
                self.init = if self.need_icw4 { Init::Icw4 } else { Init::Idle };
            }
            Init::Icw4 => {
                self.auto_eoi = v & 0x02 != 0;
                self.init = Init::Idle;
            }
        }
    }

    const fn command_read(&self) -> u8 {
        if self.read_isr { self.isr } else { self.irr }
    }

    /// The CPU accepted `line`: it moves from requested to in-service (and
    /// straight out again under auto-EOI).
    fn accept(&mut self, line: u8) {
        self.irr &= !(1 << line);
        if !self.auto_eoi {
            self.isr |= 1 << line;
        }
    }
}

/// The master and its cascaded slave: IRQ0-7 on the master, IRQ8-15 on the
/// slave, whose INT output is the master's IR2.
#[derive(Clone, Copy, Debug)]
pub struct PicPair {
    master: Pic,
    slave: Pic,
}

impl Default for PicPair {
    fn default() -> Self {
        Self::new()
    }
}

impl PicPair {
    /// Both chips as reset: everything masked, vector base 0.
    #[must_use]
    pub const fn new() -> Self {
        Self { master: Pic::new(), slave: Pic::new() }
    }

    /// An edge on interrupt line `irq` (`0..16`): latch it in the IRR.
    pub fn raise(&mut self, irq: u8) {
        if irq < 8 {
            self.master.irr |= 1 << irq;
        } else if irq < 16 {
            self.slave.irr |= 1 << (irq - 8);
        }
    }

    /// The master's request lines: its own latched edges, plus IR2 as a level
    /// that follows the slave's INT.
    const fn master_requests(&self) -> u8 {
        let slave_int = self.slave.highest(self.slave.irr).is_some();
        (self.master.irr & !0x04) | ((slave_int as u8) << 2)
    }

    /// The vector to offer the CPU, if an unmasked request outranks everything
    /// in service.
    #[must_use]
    pub const fn pending(&self) -> Option<u8> {
        match self.master.highest(self.master_requests()) {
            Some(2) => match self.slave.highest(self.slave.irr) {
                Some(line) => Some(self.slave.base + line),
                // The slave withdrew its request: the 8259A's spurious IRQ7.
                None => Some(self.master.base + 7),
            },
            Some(line) => Some(self.master.base + line),
            None => None,
        }
    }

    /// The CPU took the interrupt [`pending`](Self::pending) offered: perform
    /// the INTA cycle. Returns the vector (the same one), or `None` if nothing
    /// was pending.
    pub fn acknowledge(&mut self) -> Option<u8> {
        let vec = self.pending()?;
        match self.master.highest(self.master_requests()) {
            Some(2) => {
                if let Some(line) = self.slave.highest(self.slave.irr) {
                    self.slave.accept(line);
                }
                self.master.isr |= if self.master.auto_eoi { 0 } else { 0x04 };
            }
            Some(line) => self.master.accept(line),
            None => {}
        }
        Some(vec)
    }

    /// Read a PIC port: `0x20/0x21` master, `0xA0/0xA1` slave. `None` for any
    /// other port.
    #[must_use]
    pub const fn read(&self, port: u16) -> Option<u8> {
        match port {
            0x20 => Some(self.master.command_read()),
            0x21 => Some(self.master.imr),
            0xA0 => Some(self.slave.command_read()),
            0xA1 => Some(self.slave.imr),
            _ => None,
        }
    }

    /// Write a PIC port. Returns whether `port` was one of the four.
    pub fn write(&mut self, port: u16, v: u8) -> bool {
        match port {
            0x20 => self.master.command_write(v),
            0x21 => self.master.data_write(v),
            0xA0 => self.slave.command_write(v),
            0xA1 => self.slave.data_write(v),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The initialisation a PC kernel performs: edge, cascade, 8086 mode,
    /// bases `m` and `s`, then the given masks.
    fn init(p: &mut PicPair, m: u8, s: u8, mask_m: u8, mask_s: u8) {
        p.write(0x20, 0x11);
        p.write(0x21, m);
        p.write(0x21, 0x04);
        p.write(0x21, 0x01);
        p.write(0xA0, 0x11);
        p.write(0xA1, s);
        p.write(0xA1, 0x02);
        p.write(0xA1, 0x01);
        p.write(0x21, mask_m);
        p.write(0xA1, mask_s);
    }

    #[test]
    fn reset_state_is_fully_masked_and_silent() {
        let mut p = PicPair::new();
        assert_eq!(p.read(0x21), Some(0xFF));
        assert_eq!(p.read(0xA1), Some(0xFF));
        p.raise(0);
        assert_eq!(p.pending(), None);
    }

    #[test]
    fn initialisation_sets_the_vector_bases_and_clears_the_mask() {
        let mut p = PicPair::new();
        p.write(0x20, 0x11);
        assert_eq!(p.read(0x21), Some(0), "ICW1 clears the mask");
        p.write(0x21, 0x30 | 5); // low bits of the base are ignored
        p.write(0x21, 0x04);
        p.write(0x21, 0x01);
        p.raise(1);
        assert_eq!(p.pending(), Some(0x31));
        // After ICW4 the data port is the mask again.
        p.write(0x21, 0b10);
        assert_eq!(p.pending(), None);
        assert_eq!(p.read(0x21), Some(0b10));
    }

    #[test]
    fn a_timer_edge_is_offered_taken_and_blocked_until_eoi() {
        let mut p = PicPair::new();
        init(&mut p, 0x30, 0x38, 0xFE, 0xFF); // only IRQ0 unmasked
        p.raise(0);
        assert_eq!(p.pending(), Some(0x30));
        assert_eq!(p.acknowledge(), Some(0x30));
        // In service: a second edge is latched but not offered.
        p.raise(0);
        assert_eq!(p.pending(), None);
        // OCW3 reads the registers.
        p.write(0x20, 0x0B);
        assert_eq!(p.read(0x20), Some(0x01), "ISR");
        p.write(0x20, 0x0A);
        assert_eq!(p.read(0x20), Some(0x01), "IRR holds the second edge");
        // Non-specific EOI releases it.
        p.write(0x20, 0x20);
        assert_eq!(p.pending(), Some(0x30));
    }

    #[test]
    fn lower_lines_wait_for_higher_ones_in_service() {
        let mut p = PicPair::new();
        init(&mut p, 0x30, 0x38, 0x00, 0xFF);
        p.raise(4);
        assert_eq!(p.acknowledge(), Some(0x34));
        p.raise(5);
        assert_eq!(p.pending(), None, "IRQ5 is below IRQ4, in service");
        p.raise(1);
        assert_eq!(p.pending(), Some(0x31), "IRQ1 outranks it");
        assert_eq!(p.acknowledge(), Some(0x31));
        p.write(0x20, 0x20); // EOI: the highest in service is IRQ1
        assert_eq!(p.pending(), None, "IRQ4 still in service");
        p.write(0x20, 0x20);
        assert_eq!(p.pending(), Some(0x35));
    }

    #[test]
    fn specific_eoi_clears_the_named_line() {
        let mut p = PicPair::new();
        init(&mut p, 0x30, 0x38, 0x00, 0xFF);
        p.raise(4);
        p.acknowledge();
        p.raise(1);
        p.acknowledge();
        p.write(0x20, 0x60 | 4); // specific EOI for IRQ4
        p.write(0x20, 0x0B);
        assert_eq!(p.read(0x20), Some(0x02), "IRQ1 left in service");
    }

    #[test]
    fn a_slave_interrupt_goes_through_irq2_and_needs_two_eois() {
        let mut p = PicPair::new();
        init(&mut p, 0x30, 0x38, 0xFB, 0x00); // IRQ2 and all slave lines open
        p.raise(8 + 6); // IRQ14
        assert_eq!(p.pending(), Some(0x3E));
        assert_eq!(p.acknowledge(), Some(0x3E));
        p.write(0xA0, 0x0B);
        assert_eq!(p.read(0xA0), Some(0x40));
        p.write(0x20, 0x0B);
        assert_eq!(p.read(0x20), Some(0x04), "cascade in service on the master");
        p.write(0xA0, 0x20);
        p.write(0x20, 0x20);
        p.write(0x20, 0x0B);
        assert_eq!(p.read(0x20), Some(0));
        assert_eq!(p.pending(), None);
    }

    #[test]
    fn a_masked_slave_line_raises_nothing() {
        let mut p = PicPair::new();
        init(&mut p, 0x30, 0x38, 0xFB, 0xFF);
        p.raise(9);
        assert_eq!(p.pending(), None);
        p.write(0xA1, 0xFD); // unmask IRQ9
        assert_eq!(p.pending(), Some(0x39));
    }

    #[test]
    fn auto_eoi_never_holds_a_line_in_service() {
        let mut p = PicPair::new();
        p.write(0x20, 0x11);
        p.write(0x21, 0x30);
        p.write(0x21, 0x04);
        p.write(0x21, 0x03); // 8086 + auto-EOI
        p.write(0x21, 0xFE);
        p.raise(0);
        assert_eq!(p.acknowledge(), Some(0x30));
        p.raise(0);
        assert_eq!(p.pending(), Some(0x30));
    }

    #[test]
    fn unrelated_ports_are_not_ours() {
        let mut p = PicPair::new();
        assert_eq!(p.read(0x22), None);
        assert!(!p.write(0x40, 1));
    }
}
