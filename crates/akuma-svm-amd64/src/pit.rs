//! An Intel 8254 programmable interval timer, as a pure function of time.
//!
//! A guest kernel with no local APIC timer calibrates its time-stamp counter
//! against channel 2 (gate and output on system control port B, `0x61`) and
//! takes its tick from channel 0 on IRQ0. The model owns no clock: every entry
//! point is handed `now`, a count of the 1.193182 MHz input clock
//! ([`PIT_HZ`]) since any fixed origin, so the whole device is a deterministic
//! function of its writes and `now` — and host-testable with no hypervisor.
//!
//! Registers (`0x40` channel 0, `0x41` channel 1, `0x42` channel 2, `0x43`
//! control), the control word, the counter latch and the read-back command are
//! the 8254 datasheet's. Divergences, each deliberate:
//!
//! * **Binary counting only.** The BCD bit is accepted and ignored.
//! * **Modes 1 and 5** (hardware-triggered) run from the gate's rising edge;
//!   channels 0 and 1 have no gate input wired in a PC, so they never start.
//! * **Mode 3** reads back the count as `reload - phase`, not the datasheet's
//!   decrement-by-two; its output waveform is exact.
//! * A new count written to a running channel **restarts** it at once; the
//!   datasheet lets modes 2 and 3 finish the current period first.
//! * Channel 1 (DRAM refresh in a PC) exists as a register and nothing more.

/// The input clock: 14.31818 MHz / 12.
pub const PIT_HZ: u64 = 1_193_182;

/// PIT input-clock ticks for `us` microseconds.
#[must_use]
pub const fn ticks_from_us(us: u64) -> u64 {
    us * PIT_HZ / 1_000_000
}

/// Microseconds (rounded up) for `ticks` input-clock ticks.
#[must_use]
pub const fn us_from_ticks(ticks: u64) -> u64 {
    (ticks * 1_000_000).div_ceil(PIT_HZ)
}

/// How a channel's count register is read and written (control word bits 5:4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Lo,
    Hi,
    LoHi,
}

/// Pins and latches of the chip; each bool is one hardware flip-flop.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct Channel {
    mode: u8,
    access: Access,
    /// The programmed count; `0` means 65 536.
    reload: u32,
    /// Whether a count has been loaded since the last control word.
    loaded: bool,
    /// The tick at which the current count started (for a gated mode that has
    /// been paused, this is moved forward on resume so `now - start` is right).
    start: u64,
    /// Elapsed ticks while the gate held the counter, if it is held.
    frozen: Option<u64>,
    /// For modes 1 and 5: the trigger has not yet arrived.
    waiting_trigger: bool,
    gate: bool,
    /// Edges already reported by [`Pit::take_irq0`] since the last load.
    edges_taken: u64,
    /// First byte of a `LoHi` write, waiting for its high byte.
    write_lo: Option<u8>,
    /// Next `LoHi` read returns the high byte.
    read_hi: bool,
    /// A latched count (from the latch command or read-back), not yet read.
    latched: Option<u16>,
    /// A latched status byte (read-back), not yet read.
    status_latch: Option<u8>,
    /// Control word written and the count not yet transferred.
    null_count: bool,
}

impl Channel {
    const fn new() -> Self {
        Self {
            mode: 3,
            access: Access::LoHi,
            reload: 0x1_0000,
            loaded: false,
            start: 0,
            frozen: None,
            waiting_trigger: false,
            gate: true,
            edges_taken: 0,
            write_lo: None,
            read_hi: false,
            latched: None,
            status_latch: None,
            null_count: false,
        }
    }

    /// Input-clock ticks counted since the load, honouring a held gate.
    fn elapsed(&self, now: u64) -> u64 {
        if !self.loaded || self.waiting_trigger {
            return 0;
        }
        self.frozen.unwrap_or_else(|| now.saturating_sub(self.start))
    }

    fn load(&mut self, now: u64, count: u16) {
        self.reload = if count == 0 { 0x1_0000 } else { u32::from(count) };
        self.loaded = true;
        self.null_count = false;
        self.edges_taken = 0;
        self.start = now;
        self.frozen = None;
        self.waiting_trigger = matches!(self.mode, 1 | 5);
        // Modes 0, 2, 3, 4 count only while the gate is high.
        if !self.gate && matches!(self.mode, 0 | 2 | 3 | 4) {
            self.frozen = Some(0);
        }
    }

    /// The output pin.
    fn out(&self, now: u64) -> bool {
        let r = u64::from(self.reload);
        if !self.loaded {
            // Before the first count: mode 0 low, the others high.
            return self.mode != 0;
        }
        let e = self.elapsed(now);
        match self.mode {
            // Low from the load until the count reaches zero, then high.
            0 => e >= r,
            // High; low from the trigger until zero. Waiting for a trigger: high.
            1 => self.waiting_trigger || e >= r,
            // High, with one low clock before each reload. Held gate: high.
            2 => self.frozen.is_some() || e % r != r - 1 || r == 1,
            // Square wave: high for ceil(r/2), low for floor(r/2). Held gate: high.
            3 => self.frozen.is_some() || e % r < r.div_ceil(2),
            // High; one low clock at terminal count.
            4 => e != r,
            // As 4, from a trigger.
            _ => self.waiting_trigger || e != r,
        }
    }

    /// The count register as a counter latch would capture it.
    fn count(&self, now: u64) -> u16 {
        let r = u64::from(self.reload);
        let e = self.elapsed(now);
        let c = match self.mode {
            2 => r - e % r,
            3 => r - e % r,
            // Modes 0, 1, 4, 5 keep counting (wrapping) after terminal count.
            _ => (r.wrapping_sub(e)) & 0xFFFF,
        };
        // 65 536 reads as zero.
        (c & 0xFFFF) as u16
    }

    fn status(&self, now: u64) -> u8 {
        let rw = match self.access {
            Access::Lo => 1,
            Access::Hi => 2,
            Access::LoHi => 3,
        };
        (u8::from(self.out(now)) << 7) | (u8::from(self.null_count) << 6) | (rw << 4) | (self.mode << 1)
    }

    /// Output rising edges since the last load, as the PIC's IRQ0 line sees
    /// them (a one-shot's single rise; a periodic mode's one per period).
    fn edges(&self, now: u64) -> u64 {
        if !self.loaded || self.waiting_trigger {
            return 0;
        }
        let r = u64::from(self.reload);
        let e = self.elapsed(now);
        match self.mode {
            0 | 1 => u64::from(e >= r),
            2 | 3 => e / r,
            // 4 and 5 pulse low at terminal count and rise one clock later.
            _ => u64::from(e > r),
        }
    }

    /// The tick at which the next un-reported edge occurs, if any can.
    fn next_edge_tick(&self) -> Option<u64> {
        if !self.loaded || self.waiting_trigger || self.frozen.is_some() {
            return None;
        }
        let r = u64::from(self.reload);
        match self.mode {
            0 | 1 => (self.edges_taken == 0).then_some(self.start + r),
            2 | 3 => Some(self.start + (self.edges_taken + 1) * r),
            _ => (self.edges_taken == 0).then_some(self.start + r + 1),
        }
    }

    fn set_gate(&mut self, gate: bool, now: u64) {
        if gate == self.gate {
            return;
        }
        self.gate = gate;
        if !self.loaded {
            return;
        }
        if gate {
            // Rising edge.
            match self.mode {
                // Resume where it was held.
                0 | 4 => {
                    if let Some(f) = self.frozen.take() {
                        self.start = now.saturating_sub(f);
                    }
                }
                // Reload and run; modes 1/5 are triggered for the first time.
                _ => {
                    self.waiting_trigger = false;
                    self.frozen = None;
                    self.start = now;
                    self.edges_taken = 0;
                }
            }
        } else if matches!(self.mode, 0 | 2 | 3 | 4) && self.frozen.is_none() && !self.waiting_trigger {
            self.frozen = Some(self.elapsed(now));
        }
    }
}

/// The 8254 and the system control port B bits that wire channel 2 to the
/// outside world.
#[derive(Clone, Copy, Debug)]
pub struct Pit {
    ch: [Channel; 3],
    /// Port `0x61` bit 1: speaker data enable (remembered, nothing sounds).
    speaker: bool,
}

impl Default for Pit {
    fn default() -> Self {
        Self::new()
    }
}

impl Pit {
    /// Power-on state: every channel in mode 3 with no count loaded.
    #[must_use]
    pub const fn new() -> Self {
        let mut c = [Channel::new(); 3];
        // Channels 0 and 1 have their gates tied high; channel 2's follows
        // port B bit 0 and is low at power-on.
        c[2].gate = false;
        Self { ch: c, speaker: false }
    }

    /// A read of PIT port `reg` (`0..=3`) at tick `now`.
    pub fn read(&mut self, reg: u8, now: u64) -> u8 {
        let Some(ch) = self.ch.get_mut(usize::from(reg & 3)) else {
            // Reading the control register: write-only, bus floats.
            return 0xFF;
        };
        if let Some(s) = ch.status_latch.take() {
            return s;
        }
        let access = ch.access;
        let value = ch.latched.unwrap_or_else(|| ch.count(now));
        match access {
            Access::Lo => {
                ch.latched = None;
                value as u8
            }
            Access::Hi => {
                ch.latched = None;
                (value >> 8) as u8
            }
            Access::LoHi => {
                if ch.read_hi {
                    ch.read_hi = false;
                    ch.latched = None;
                    (value >> 8) as u8
                } else {
                    ch.read_hi = true;
                    value as u8
                }
            }
        }
    }

    /// A write of `val` to PIT port `reg` (`0..=3`) at tick `now`.
    pub fn write(&mut self, reg: u8, val: u8, now: u64) {
        let reg = reg & 3;
        if reg == 3 {
            self.control(val, now);
            return;
        }
        let ch = &mut self.ch[usize::from(reg)];
        match ch.access {
            Access::Lo => ch.load(now, u16::from(val)),
            Access::Hi => ch.load(now, u16::from(val) << 8),
            Access::LoHi => match ch.write_lo.take() {
                None => {
                    ch.write_lo = Some(val);
                    // Mode 0 stops counting on the first byte.
                    if ch.mode == 0 {
                        ch.loaded = false;
                    }
                }
                Some(lo) => ch.load(now, u16::from(lo) | (u16::from(val) << 8)),
            },
        }
    }

    fn control(&mut self, val: u8, now: u64) {
        let sc = val >> 6;
        if sc == 3 {
            // Read-back: bit 5 /COUNT, bit 4 /STATUS, bits 3:1 select 2,1,0.
            for (i, ch) in self.ch.iter_mut().enumerate() {
                if val & (2 << i) == 0 {
                    continue;
                }
                if val & 0x10 == 0 && ch.status_latch.is_none() {
                    ch.status_latch = Some(ch.status(now));
                }
                if val & 0x20 == 0 && ch.latched.is_none() {
                    ch.latched = Some(ch.count(now));
                }
            }
            return;
        }
        let ch = &mut self.ch[usize::from(sc)];
        let rw = (val >> 4) & 3;
        if rw == 0 {
            // Counter latch command.
            if ch.latched.is_none() {
                ch.latched = Some(ch.count(now));
            }
            return;
        }
        ch.access = match rw {
            1 => Access::Lo,
            2 => Access::Hi,
            _ => Access::LoHi,
        };
        // Modes 6 and 7 are aliases of 2 and 3.
        ch.mode = match (val >> 1) & 7 {
            6 => 2,
            7 => 3,
            m => m,
        };
        ch.loaded = false;
        ch.waiting_trigger = false;
        ch.frozen = None;
        ch.write_lo = None;
        ch.read_hi = false;
        ch.latched = None;
        ch.status_latch = None;
        ch.null_count = true;
        ch.edges_taken = 0;
    }

    /// Port `0x61` as read: bit 0 gate 2, bit 1 speaker, bit 4 the DRAM-refresh
    /// toggle (flips about every 15 µs), bit 5 channel 2's output.
    #[must_use]
    pub fn port_b_read(&self, now: u64) -> u8 {
        let refresh = (now / 18) & 1;
        u8::from(self.ch[2].gate)
            | (u8::from(self.speaker) << 1)
            | ((refresh as u8) << 4)
            | (u8::from(self.ch[2].out(now)) << 5)
    }

    /// Port `0x61` written: bit 0 gates channel 2, bit 1 enables the speaker.
    pub fn port_b_write(&mut self, val: u8, now: u64) {
        self.speaker = val & 2 != 0;
        self.ch[2].set_gate(val & 1 != 0, now);
    }

    /// Channel 2's output pin (what port B bit 5 reports).
    #[must_use]
    pub fn out2(&self, now: u64) -> bool {
        self.ch[2].out(now)
    }

    /// Channel 0 has produced an output edge since the last call: raise IRQ0.
    /// Edges that piled up while the caller was not looking coalesce into one,
    /// as they do on an 8259 whose IRR bit is already set.
    pub fn take_irq0(&mut self, now: u64) -> bool {
        let edges = self.ch[0].edges(now);
        if edges > self.ch[0].edges_taken {
            self.ch[0].edges_taken = edges;
            true
        } else {
            false
        }
    }

    /// The tick at which channel 0 will next raise IRQ0, if it ever will.
    #[must_use]
    pub fn next_irq0_tick(&self) -> Option<u64> {
        self.ch[0].next_edge_tick()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH2: u8 = 2;

    /// Program channel `n` (mode, lo/hi access) with `count`.
    fn program(p: &mut Pit, n: u8, mode: u8, count: u16, now: u64) {
        p.write(3, (n << 6) | 0x30 | (mode << 1), now);
        p.write(n, count as u8, now);
        p.write(n, (count >> 8) as u8, now);
    }

    #[test]
    fn clock_conversions_round_trip_within_a_tick() {
        assert_eq!(ticks_from_us(1_000_000), PIT_HZ);
        assert_eq!(ticks_from_us(0), 0);
        for t in [1u64, 100, 11_932, 1_193_182] {
            assert!(ticks_from_us(us_from_ticks(t)) >= t - 1);
        }
    }

    /// The calibration shape: gate low, mode 0, count loaded, gate raised, then
    /// poll port B bit 5 until the output goes high — after exactly `count`
    /// clocks, not before.
    #[test]
    fn channel_2_mode_0_gated_countdown_sets_port_b_bit_5() {
        let mut p = Pit::new();
        p.port_b_write(0, 100);
        program(&mut p, CH2, 0, 11_932, 100); // ~10 ms
        // Loaded with the gate low: nothing counts, output low.
        assert_eq!(p.port_b_read(1_000_000) & 0x20, 0);
        // Gate up at t=2000: the countdown starts there.
        p.port_b_write(1, 2000);
        assert_eq!(p.port_b_read(2000 + 11_931) & 0x20, 0);
        assert_eq!(p.port_b_read(2000 + 11_932) & 0x20, 0x20);
        assert_eq!(p.port_b_read(2000 + 99_999) & 0x20, 0x20);
        // Port B reports the gate it was given.
        assert_eq!(p.port_b_read(2000) & 1, 1);
    }

    /// Dropping the gate in mode 0 pauses the count; raising it resumes.
    #[test]
    fn mode_0_gate_low_pauses_the_count() {
        let mut p = Pit::new();
        p.port_b_write(1, 0);
        program(&mut p, CH2, 0, 1000, 0);
        p.port_b_write(0, 400); // 400 counted, 600 to go
        assert_eq!(p.port_b_read(10_000) & 0x20, 0, "held");
        p.port_b_write(1, 10_000);
        assert_eq!(p.port_b_read(10_599) & 0x20, 0);
        assert_eq!(p.port_b_read(10_600) & 0x20, 0x20);
    }

    /// The counter-latch command freezes a value; lo then hi reads return it
    /// while the live count moves on.
    #[test]
    fn counter_latch_returns_a_stable_lo_hi_pair() {
        let mut p = Pit::new();
        p.port_b_write(1, 0);
        program(&mut p, CH2, 0, 0xFFFF, 0);
        p.write(3, 2 << 6, 1000); // latch channel 2
        let lo = p.read(CH2, 5000);
        let hi = p.read(CH2, 9000);
        assert_eq!(u16::from(hi) << 8 | u16::from(lo), 0xFFFF - 1000);
        // Unlatched: reads follow the clock again.
        let lo = p.read(CH2, 2000);
        let hi = p.read(CH2, 2000);
        assert_eq!(u16::from(hi) << 8 | u16::from(lo), 0xFFFF - 2000);
    }

    /// Count 0 is 65 536.
    #[test]
    fn count_zero_is_65536() {
        let mut p = Pit::new();
        p.port_b_write(1, 0);
        program(&mut p, CH2, 0, 0, 0);
        assert!(!p.out2(65_535));
        assert!(p.out2(65_536));
    }

    #[test]
    fn mode_3_is_a_square_wave() {
        let mut p = Pit::new();
        p.port_b_write(1, 0);
        program(&mut p, CH2, 3, 10, 0);
        // High for 5, low for 5, repeating.
        for t in 0..40u64 {
            assert_eq!(p.out2(t), t % 10 < 5, "t={t}");
        }
        // Odd count: high for 6, low for 5.
        program(&mut p, CH2, 3, 11, 100);
        let hi = (100..122).filter(|&t| p.out2(t)).count();
        assert_eq!(hi, 12, "two periods, 6 high each");
    }

    #[test]
    fn mode_2_pulses_low_for_one_clock_per_period() {
        let mut p = Pit::new();
        p.port_b_write(1, 0);
        program(&mut p, CH2, 2, 5, 0);
        let lows: [u64; 3] = [4, 9, 14];
        for t in 0..15u64 {
            assert_eq!(p.out2(t), !lows.contains(&t), "t={t}");
        }
    }

    /// Channel 0 mode 2: one IRQ0 per period, coalesced when the caller was
    /// late, and the next deadline is one period on.
    #[test]
    fn channel_0_periodic_edges() {
        let mut p = Pit::new();
        program(&mut p, 0, 2, 11_932, 1_000);
        assert!(!p.take_irq0(1_000 + 11_931));
        assert_eq!(p.next_irq0_tick(), Some(1_000 + 11_932));
        assert!(p.take_irq0(1_000 + 11_932));
        assert!(!p.take_irq0(1_000 + 11_933), "one edge, taken once");
        assert_eq!(p.next_irq0_tick(), Some(1_000 + 2 * 11_932));
        // Late by five periods: still a single interrupt.
        assert!(p.take_irq0(1_000 + 7 * 11_932));
        assert!(!p.take_irq0(1_000 + 7 * 11_932));
        assert_eq!(p.next_irq0_tick(), Some(1_000 + 8 * 11_932));
    }

    /// Channel 0 mode 0 (a one-shot): exactly one edge, then silence.
    #[test]
    fn channel_0_one_shot_fires_once() {
        let mut p = Pit::new();
        program(&mut p, 0, 0, 500, 0);
        assert_eq!(p.next_irq0_tick(), Some(500));
        assert!(!p.take_irq0(499));
        assert!(p.take_irq0(500));
        assert!(!p.take_irq0(1_000_000));
        assert_eq!(p.next_irq0_tick(), None);
        // Re-arming starts a fresh shot.
        program(&mut p, 0, 0, 500, 2_000_000);
        assert!(p.take_irq0(2_000_500));
    }

    /// An unprogrammed channel 0 never interrupts.
    #[test]
    fn unprogrammed_channel_is_silent() {
        let mut p = Pit::new();
        assert!(!p.take_irq0(u64::MAX / 4));
        assert_eq!(p.next_irq0_tick(), None);
    }

    /// Read-back: status then count, with the status layout of the datasheet
    /// (OUT, null count, RW, mode, BCD).
    #[test]
    fn read_back_status_and_count() {
        let mut p = Pit::new();
        p.port_b_write(1, 0);
        p.write(3, (2 << 6) | 0x30 | (2 << 1), 0); // ch2, lo/hi, mode 2
        // Control written, count not yet loaded: null count set.
        p.write(3, 0xC0 | 0x20 | 0x08, 0); // read-back ch2, status only
        assert_eq!(p.read(CH2, 0), 0x80 | 0x40 | 0x30 | (2 << 1));
        p.write(CH2, 0x10, 0);
        p.write(CH2, 0x00, 0);
        p.write(3, 0xC0 | 0x10 | 0x08, 0); // count only
        assert_eq!(p.read(CH2, 0), 0x10);
        assert_eq!(p.read(CH2, 0), 0x00);
        p.write(3, 0xC0 | 0x20 | 0x08, 5);
        assert_eq!(p.read(CH2, 5) & 0x40, 0, "null count cleared by the load");
    }

    /// Mode 1 starts only on the gate's rising edge.
    #[test]
    fn mode_1_is_triggered_by_the_gate() {
        let mut p = Pit::new();
        p.port_b_write(0, 0);
        program(&mut p, CH2, 1, 100, 0);
        assert!(p.out2(10_000), "idle output is high");
        p.port_b_write(1, 1000);
        assert!(!p.out2(1050), "low while counting");
        assert!(p.out2(1100));
    }

    /// Port B's refresh bit toggles on its own, every ~15 us.
    #[test]
    fn port_b_refresh_bit_toggles() {
        let p = Pit::new();
        let a = p.port_b_read(0) & 0x10;
        let b = p.port_b_read(18) & 0x10;
        assert_ne!(a, b);
    }
}
