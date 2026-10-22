//! A local APIC in xAPIC (MMIO) mode: the register page, the timer, and the
//! interrupt request/in-service state a hypervisor needs to hand a guest its tick.
//!
//! The guest that wants one is Akuma itself (`amd64/src/lapic.rs`): it maps the
//! page at `0xFEE0_0000`, software-enables it, and drives a calibrated periodic or
//! one-shot timer on LVT vector 32. Everything else about the chip — thermal and
//! performance LVTs, the error register, logical destination — is present as
//! storage so a driver that writes them reads them back, and has no effect.
//!
//! # The model
//!
//! The device delivers nothing itself, like [`crate::pic::PicPair`]. The caller
//! owns time and passes it in, in **bus-clock ticks** ([`BUS_HZ`], one per
//! nanosecond), so every answer is a function of `now` and the model has no
//! hidden clock:
//!
//! * [`Lapic::poll`] folds timer expiries up to `now` into the request register;
//! * [`Lapic::pending`] is the vector to offer the CPU — the highest request whose
//!   priority class beats the processor priority — or `None`;
//! * [`Lapic::acknowledge`] is the CPU taking it (request moves to in-service);
//! * a guest write to the EOI register retires the highest in-service vector.
//!
//! The timer counts down once per `divide` bus ticks from the initial count,
//! raises its LVT vector at zero, and in periodic mode reloads. Expiries the
//! caller was too late to see **coalesce** into one request, as a real request bit
//! that is already set does. A masked LVT entry keeps counting and raises nothing.
//!
//! Not modelled: x2APIC, the TSC-deadline timer, directed inter-processor
//! interrupts (a fixed IPI to this processor — itself or by shorthand — is
//! delivered; anything else has no target and is dropped), NMI/SMI/INIT/ExtINT
//! delivery, level-triggered requests (TMR reads zero) and the APIC-error logic
//! beyond an illegal-vector report.
//!
//! Register offsets and bit positions are the architectural xAPIC register map
//! (AMD APM Vol 2, ch. 16 "Local APIC"); the tests pin every offset numerically
//! rather than through the constants, so a transposed digit has to be made twice.

/// Where the register page sits at reset (`IA32_APIC_BASE[51:12]`); the guest
/// may relocate it with a `WRMSR`.
pub const DEFAULT_BASE: u64 = 0xFEE0_0000;
/// The register page is 4 KiB.
pub const PAGE_LEN: u64 = 0x1000;

/// The bus clock the timer divides: 1 GHz.
///
/// One `now` unit is one nanosecond. Any value works for a guest that calibrates its timer against the PIT, which
/// Akuma's does; a round number makes the arithmetic auditable.
pub const BUS_HZ: u64 = 1_000_000_000;

/// Bus ticks for `us` microseconds of the caller's clock.
#[must_use]
pub const fn bus_ticks_from_us(us: u64) -> u64 {
    us * (BUS_HZ / 1_000_000)
}

/// Register offsets.
pub mod reg {
    pub const ID: u64 = 0x020;
    pub const VERSION: u64 = 0x030;
    pub const TPR: u64 = 0x080;
    pub const APR: u64 = 0x090;
    pub const PPR: u64 = 0x0A0;
    pub const EOI: u64 = 0x0B0;
    pub const RRD: u64 = 0x0C0;
    pub const LDR: u64 = 0x0D0;
    pub const DFR: u64 = 0x0E0;
    pub const SVR: u64 = 0x0F0;
    /// First of eight 16-byte-spaced dwords, one per 32 vectors.
    pub const ISR: u64 = 0x100;
    pub const TMR: u64 = 0x180;
    pub const IRR: u64 = 0x200;
    pub const ESR: u64 = 0x280;
    pub const LVT_CMCI: u64 = 0x2F0;
    pub const ICR_LOW: u64 = 0x300;
    pub const ICR_HIGH: u64 = 0x310;
    pub const LVT_TIMER: u64 = 0x320;
    pub const LVT_THERMAL: u64 = 0x330;
    pub const LVT_PERF: u64 = 0x340;
    pub const LVT_LINT0: u64 = 0x350;
    pub const LVT_LINT1: u64 = 0x360;
    pub const LVT_ERROR: u64 = 0x370;
    pub const TIMER_INIT: u64 = 0x380;
    pub const TIMER_CUR: u64 = 0x390;
    pub const TIMER_DIV: u64 = 0x3E0;
}

/// SVR bit 8: the APIC is software-enabled. Clear, it accepts no interrupts.
pub const SVR_ENABLE: u32 = 1 << 8;
/// LVT bit 16: masked.
pub const LVT_MASKED: u32 = 1 << 16;
/// LVT timer bits 18:17 = 01: periodic (00 one-shot; 10 is TSC-deadline, not
/// modelled and read as one-shot).
const LVT_TIMER_MODE_SHIFT: u32 = 17;
const LVT_TIMER_PERIODIC: u32 = 1;

/// Version register: version `0x14` (a discrete-era xAPIC), `MaxLVT` = 5 (six
/// entries, 0..=5, counting CMCI).
const VERSION: u32 = 0x0005_0014;
/// ESR bit 6: the processor was asked to accept an illegal vector (< 16).
const ESR_ILLEGAL_VECTOR_RECEIVED: u32 = 1 << 6;

/// Which register the 3-bit divide field `0b0?11`-style encoding selects: the
/// timer divides the bus clock by 2, 4, 8, 16, 32, 64, 128 or 1 for values
/// 0,1,2,3,8,9,10,11 (bit 2 is reserved and ignored).
#[must_use]
pub const fn divide_by(div_reg: u32) -> u64 {
    // Bits 3 and 1:0 form the index; bit 2 is reserved.
    let idx = ((div_reg >> 1) & 0b100) | (div_reg & 0b11);
    if idx == 0b111 { 1 } else { 2 << idx }
}

/// The local APIC.
#[derive(Clone, Copy, Debug)]
pub struct Lapic {
    id: u8,
    svr: u32,
    tpr: u8,
    ldr: u32,
    dfr: u32,
    esr: u32,
    icr_high: u32,
    lvt_cmci: u32,
    lvt_timer: u32,
    lvt_thermal: u32,
    lvt_perf: u32,
    lvt_lint0: u32,
    lvt_lint1: u32,
    lvt_error: u32,
    timer_init: u32,
    timer_div: u32,
    /// Bus tick at which the running count was last loaded.
    timer_start: u64,
    /// Bus tick of the next expiry, or `None` when stopped.
    timer_next: Option<u64>,
    irr: [u32; 8],
    isr: [u32; 8],
    /// The last ICR write that names other processors, until [`Lapic::take_ipi`].
    outbox: Option<Ipi>,
}

/// What an ICR write asks of the processors it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiMode {
    /// Delivery mode 000: raise `vector`.
    Fixed,
    /// Delivery mode 101, level assert: reset the target and park it until a STARTUP.
    Init,
    /// Delivery mode 110: start a parked target in real mode at `vector << 12`.
    Startup,
}

/// Who an ICR write names, after the destination shorthand (bits 19:18) is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiDest {
    /// Physical destination: the APIC id.
    Id(u8),
    /// Shorthand "all excluding self".
    AllButSelf,
    /// Shorthand "all including self" (the sender already took its own copy).
    All,
}

/// An inter-processor interrupt on its way from one local APIC to the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipi {
    pub mode: IpiMode,
    pub vector: u8,
    pub dest: IpiDest,
}

impl Ipi {
    /// Does the processor with APIC id `id`, which is not the sender, receive this?
    #[must_use]
    pub const fn reaches(&self, id: u8) -> bool {
        match self.dest {
            IpiDest::Id(d) => d == id,
            IpiDest::AllButSelf | IpiDest::All => true,
        }
    }
}

impl Default for Lapic {
    fn default() -> Self {
        Self::new()
    }
}

impl Lapic {
    /// A reset local APIC: software-disabled, every LVT entry masked.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            id: 0,
            svr: 0xFF,
            tpr: 0,
            ldr: 0,
            dfr: 0xFFFF_FFFF,
            esr: 0,
            icr_high: 0,
            lvt_cmci: LVT_MASKED,
            lvt_timer: LVT_MASKED,
            lvt_thermal: LVT_MASKED,
            lvt_perf: LVT_MASKED,
            lvt_lint0: LVT_MASKED,
            lvt_lint1: LVT_MASKED,
            lvt_error: LVT_MASKED,
            timer_init: 0,
            timer_div: 0,
            timer_start: 0,
            timer_next: None,
            irr: [0; 8],
            isr: [0; 8],
            outbox: None,
        }
    }

    /// This unit with APIC id `id`.
    #[must_use]
    pub const fn with_id(mut self, id: u8) -> Self {
        self.id = id;
        self
    }

    /// The processor's APIC id (the ID register's bits 31:24).
    #[must_use]
    pub const fn id(&self) -> u8 {
        self.id
    }

    /// Is the unit software-enabled (SVR bit 8)?
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.svr & SVR_ENABLE != 0
    }

    /// The bus ticks one count of the timer takes.
    fn period_ticks(&self) -> u64 {
        divide_by(self.timer_div)
    }

    fn periodic(&self) -> bool {
        (self.lvt_timer >> LVT_TIMER_MODE_SHIFT) & 0b11 == LVT_TIMER_PERIODIC
    }

    /// The timer's vector, if its LVT entry is unmasked.
    fn timer_vector(&self) -> Option<u8> {
        (self.lvt_timer & LVT_MASKED == 0).then_some(self.lvt_timer as u8)
    }

    // ----- interrupt request / in-service state ------------------------------

    fn bit(words: &[u32; 8], vector: u8) -> bool {
        words[usize::from(vector >> 5)] & (1 << (vector & 31)) != 0
    }

    fn set_bit(words: &mut [u32; 8], vector: u8) {
        words[usize::from(vector >> 5)] |= 1 << (vector & 31);
    }

    fn clear_bit(words: &mut [u32; 8], vector: u8) {
        words[usize::from(vector >> 5)] &= !(1 << (vector & 31));
    }

    fn highest(words: &[u32; 8]) -> Option<u8> {
        (0..8u8).rev().find_map(|w| {
            let word = words[usize::from(w)];
            (word != 0).then(|| w * 32 + (31 - word.leading_zeros() as u8))
        })
    }

    /// Request `vector` (a fixed-delivery interrupt aimed at this processor).
    /// Vectors below 16 are architecturally illegal: no request, and the error
    /// register records it.
    pub fn raise(&mut self, vector: u8) {
        if vector < 16 {
            self.esr |= ESR_ILLEGAL_VECTOR_RECEIVED;
            return;
        }
        Self::set_bit(&mut self.irr, vector);
    }

    /// The processor priority: the larger of the task priority and the highest
    /// in-service vector's class, as the high nibble.
    #[must_use]
    pub fn ppr(&self) -> u8 {
        let isrv = Self::highest(&self.isr).unwrap_or(0);
        if self.tpr >> 4 >= isrv >> 4 { self.tpr } else { isrv & 0xF0 }
    }

    /// The vector to offer the CPU: the highest pending request whose class is
    /// above the processor priority. `None` while software-disabled.
    #[must_use]
    pub fn pending(&self) -> Option<u8> {
        if !self.enabled() {
            return None;
        }
        let v = Self::highest(&self.irr)?;
        (v >> 4 > self.ppr() >> 4).then_some(v)
    }

    /// The CPU took `vector` (the interrupt-acknowledge): the request becomes
    /// in-service. A vector that is not pending is ignored.
    pub fn acknowledge(&mut self, vector: u8) {
        if Self::bit(&self.irr, vector) {
            Self::clear_bit(&mut self.irr, vector);
            Self::set_bit(&mut self.isr, vector);
        }
    }

    /// End of interrupt: retire the highest in-service vector, which is returned.
    pub fn eoi(&mut self) -> Option<u8> {
        let v = Self::highest(&self.isr)?;
        Self::clear_bit(&mut self.isr, v);
        Some(v)
    }

    // ----- the timer ----------------------------------------------------------

    /// Arm the timer from `now`: a zero initial count stops it.
    fn load_timer(&mut self, now: u64) {
        self.timer_start = now;
        self.timer_next = if self.timer_init == 0 {
            None
        } else {
            Some(now.saturating_add(u64::from(self.timer_init).saturating_mul(self.period_ticks())))
        };
    }

    /// The current count at `now`.
    fn current_count(&self, now: u64) -> u32 {
        // Stopped, or a one-shot that has expired: zero until it is reloaded.
        let Some(next) = self.timer_next else { return 0 };
        let period = self.period_ticks();
        // Counts still to go until the next expiry (rounded up: the count reads
        // 1 until the last bus tick before zero).
        let left = next.saturating_sub(now).div_ceil(period);
        left.min(u64::from(self.timer_init)) as u32
    }

    /// Fold every timer expiry up to `now` into the request register.
    ///
    /// Periodic: several missed periods are one request, and the next expiry
    /// lands on the original phase (`start + k * init * divide`), not on the
    /// moment the caller got round to looking.
    pub fn poll(&mut self, now: u64) {
        let Some(next) = self.timer_next else { return };
        if now < next {
            return;
        }
        if let Some(v) = self.timer_vector() {
            self.raise(v);
        }
        if self.periodic() && self.timer_init != 0 {
            let full = u64::from(self.timer_init).saturating_mul(self.period_ticks());
            let missed = (now - next) / full + 1;
            self.timer_next = Some(next.saturating_add(missed.saturating_mul(full)));
        } else {
            self.timer_next = None;
        }
    }

    /// Is LINT0 programmed as the "virtual wire" (ExtINT delivery, unmasked) on a
    /// software-enabled unit? That is how a guest with a local APIC but no I/O
    /// APIC receives the legacy 8259's interrupts: the processor runs an
    /// interrupt-acknowledge cycle to the PIC instead of taking a vector from
    /// the APIC's own request register.
    #[must_use]
    pub const fn lint0_extint(&self) -> bool {
        const DELIVERY_MODE_MASK: u32 = 0b111 << 8;
        const EXTINT: u32 = 0b111 << 8;
        self.enabled() && self.lvt_lint0 & LVT_MASKED == 0 && self.lvt_lint0 & DELIVERY_MODE_MASK == EXTINT
    }

    /// The bus tick of the next timer expiry that would raise an interrupt, for a
    /// halted guest: `None` when the timer is stopped, masked, or the unit is
    /// software-disabled. (A request already pending is [`Lapic::pending`]'s.)
    #[must_use]
    pub fn next_event(&self) -> Option<u64> {
        if !self.enabled() || self.timer_vector().is_none() {
            return None;
        }
        self.timer_next
    }

    // ----- the register page --------------------------------------------------

    /// A guest load from the register page at byte `off` (dword registers; the
    /// caller decodes and widens). Unknown registers read zero.
    #[must_use]
    pub fn read(&mut self, off: u64, now: u64) -> u32 {
        self.poll(now);
        match off {
            reg::ID => u32::from(self.id) << 24,
            reg::VERSION => VERSION,
            reg::TPR => u32::from(self.tpr),
            reg::APR | reg::PPR => u32::from(self.ppr()),
            reg::LDR => self.ldr,
            reg::DFR => self.dfr,
            reg::SVR => self.svr,
            o if (reg::ISR..reg::ISR + 0x80).contains(&o) && o % 0x10 == 0 => {
                self.isr[((o - reg::ISR) / 0x10) as usize]
            }
            o if (reg::TMR..reg::TMR + 0x80).contains(&o) && o % 0x10 == 0 => 0,
            o if (reg::IRR..reg::IRR + 0x80).contains(&o) && o % 0x10 == 0 => {
                self.irr[((o - reg::IRR) / 0x10) as usize]
            }
            reg::ESR => self.esr,
            reg::LVT_CMCI => self.lvt_cmci,
            // The delivery-status bit (12) is always clear: sends complete at once.
            reg::ICR_LOW => 0,
            reg::ICR_HIGH => self.icr_high,
            reg::LVT_TIMER => self.lvt_timer,
            reg::LVT_THERMAL => self.lvt_thermal,
            reg::LVT_PERF => self.lvt_perf,
            reg::LVT_LINT0 => self.lvt_lint0,
            reg::LVT_LINT1 => self.lvt_lint1,
            reg::LVT_ERROR => self.lvt_error,
            reg::TIMER_INIT => self.timer_init,
            reg::TIMER_CUR => self.current_count(now),
            reg::TIMER_DIV => self.timer_div,
            _ => 0,
        }
    }

    /// A guest store to the register page. Read-only registers ignore the write.
    pub fn write(&mut self, off: u64, val: u32, now: u64) {
        // Expiries before this write belong to the configuration before it.
        self.poll(now);
        match off {
            reg::ID => self.id = (val >> 24) as u8,
            reg::TPR => self.tpr = val as u8,
            reg::EOI => {
                self.eoi();
            }
            reg::LDR => self.ldr = val,
            reg::DFR => self.dfr = val,
            reg::SVR => self.svr = val,
            // The error register is write-to-latch: the written value is ignored
            // and the accumulated errors are cleared.
            reg::ESR => self.esr = 0,
            reg::LVT_CMCI => self.lvt_cmci = val,
            reg::ICR_HIGH => self.icr_high = val,
            reg::ICR_LOW => self.send_ipi(val),
            reg::LVT_TIMER => self.lvt_timer = val,
            reg::LVT_THERMAL => self.lvt_thermal = val,
            reg::LVT_PERF => self.lvt_perf = val,
            reg::LVT_LINT0 => self.lvt_lint0 = val,
            reg::LVT_LINT1 => self.lvt_lint1 = val,
            reg::LVT_ERROR => self.lvt_error = val,
            reg::TIMER_INIT => {
                self.timer_init = val;
                self.load_timer(now);
            }
            reg::TIMER_DIV => {
                // A divisor change on a running timer keeps the remaining count
                // and re-times it at the new rate.
                let left = self.current_count(now);
                self.timer_div = val & 0b1011;
                if self.timer_next.is_some() {
                    self.timer_next =
                        Some(now.saturating_add(u64::from(left.max(1)).saturating_mul(self.period_ticks())));
                }
            }
            _ => {}
        }
    }

    /// An ICR write. A fixed IPI that includes this processor is delivered to it
    /// here; the whole request (the sender's copy aside) waits in the outbox for
    /// the machine to route to the others. Destination shorthand (bits 19:18): 00
    /// none (the destination is `icr_high[31:24]`), 01 self, 10 all including self,
    /// 11 all excluding self. Modes other than fixed, INIT and STARTUP, and an INIT
    /// level *de*-assert, do nothing.
    fn send_ipi(&mut self, low: u32) {
        const LEVEL: u32 = 1 << 15;
        const ASSERT: u32 = 1 << 14;
        let mode = match (low >> 8) & 0b111 {
            0b000 => IpiMode::Fixed,
            0b101 if low & LEVEL == 0 || low & ASSERT != 0 => IpiMode::Init,
            0b110 => IpiMode::Startup,
            _ => return,
        };
        let vector = low as u8;
        let dest = match (low >> 18) & 0b11 {
            0b00 => IpiDest::Id((self.icr_high >> 24) as u8),
            0b01 => IpiDest::Id(self.id),
            0b10 => IpiDest::All,
            _ => IpiDest::AllButSelf,
        };
        let hits_self = match dest {
            IpiDest::Id(d) => d == self.id,
            IpiDest::All => true,
            IpiDest::AllButSelf => false,
        };
        if hits_self && mode == IpiMode::Fixed {
            self.raise(vector);
        }
        // A request that names only this processor has nobody to route to.
        if !matches!(dest, IpiDest::Id(d) if d == self.id) {
            self.outbox = Some(Ipi { mode, vector, dest });
        }
    }

    /// The IPI the last ICR write left for the other processors, if any.
    pub fn take_ipi(&mut self) -> Option<Ipi> {
        self.outbox.take()
    }

    /// An INIT: the unit returns to its reset state (software-disabled, every LVT
    /// masked, no requests), keeping its APIC id.
    pub fn init_reset(&mut self) {
        *self = Self::new().with_id(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: u64 = 1_000; // bus ticks per microsecond

    fn enabled() -> Lapic {
        let mut l = Lapic::new();
        l.write(0xF0, 0x1FF, 0);
        l
    }

    /// Program the timer like Akuma's `start_timer`: divide 16, periodic,
    /// vector 32, then the initial count.
    fn start_periodic(l: &mut Lapic, count: u32, now: u64) {
        l.write(0x3E0, 0b0011, now);
        l.write(0x320, 32 | (1 << 17), now);
        l.write(0x380, count, now);
    }

    #[test]
    fn the_register_offsets_are_the_architectural_ones() {
        // Numeric, not through `reg::`: the architectural xAPIC map.
        assert_eq!(reg::ID, 0x20);
        assert_eq!(reg::VERSION, 0x30);
        assert_eq!(reg::TPR, 0x80);
        assert_eq!(reg::PPR, 0xA0);
        assert_eq!(reg::EOI, 0xB0);
        assert_eq!(reg::SVR, 0xF0);
        assert_eq!(reg::ISR, 0x100);
        assert_eq!(reg::TMR, 0x180);
        assert_eq!(reg::IRR, 0x200);
        assert_eq!(reg::ESR, 0x280);
        assert_eq!(reg::ICR_LOW, 0x300);
        assert_eq!(reg::ICR_HIGH, 0x310);
        assert_eq!(reg::LVT_TIMER, 0x320);
        assert_eq!(reg::LVT_ERROR, 0x370);
        assert_eq!(reg::TIMER_INIT, 0x380);
        assert_eq!(reg::TIMER_CUR, 0x390);
        assert_eq!(reg::TIMER_DIV, 0x3E0);
    }

    #[test]
    fn divide_field_decodes_the_documented_encodings() {
        // 0,1,2,3 -> /2,/4,/8,/16; 8,9,10 -> /32,/64,/128; 11 -> /1.
        for (field, want) in [(0, 2), (1, 4), (2, 8), (3, 16), (8, 32), (9, 64), (10, 128), (11, 1)] {
            assert_eq!(divide_by(field), want, "field {field:#x}");
        }
        // Bit 2 is reserved.
        assert_eq!(divide_by(0b0111), 16);
    }

    #[test]
    fn reset_state_is_disabled_and_masked() {
        let mut l = Lapic::new();
        assert!(!l.enabled());
        assert_eq!(l.read(0xF0, 0), 0xFF);
        for lvt in [0x2F0, 0x320, 0x330, 0x340, 0x350, 0x360, 0x370] {
            assert_eq!(l.read(lvt, 0), LVT_MASKED, "LVT {lvt:#x}");
        }
        assert_eq!(l.read(0x30, 0) & 0xFF, 0x14);
        assert_eq!(l.pending(), None);
    }

    #[test]
    fn a_disabled_unit_offers_nothing() {
        let mut l = Lapic::new();
        l.raise(0x40);
        assert_eq!(l.pending(), None);
        l.write(0xF0, 0x1FF, 0);
        assert_eq!(l.pending(), Some(0x40));
    }

    #[test]
    fn id_register_keeps_the_id_in_the_top_byte() {
        let mut l = enabled();
        l.write(0x20, 3 << 24, 0);
        assert_eq!(l.read(0x20, 0), 3 << 24);
        assert_eq!(l.id(), 3);
    }

    #[test]
    fn periodic_timer_fires_at_count_times_divide() {
        let mut l = enabled();
        // 625 000 counts at /16 of a 1 GHz bus is 10 ms exactly.
        start_periodic(&mut l, 625_000, 0);
        assert_eq!(l.next_event(), Some(10_000 * US));
        l.poll(10_000 * US - 1);
        assert_eq!(l.pending(), None);
        l.poll(10_000 * US);
        assert_eq!(l.pending(), Some(32));
    }

    #[test]
    fn a_late_caller_sees_one_request_and_the_next_edge_keeps_its_phase() {
        let mut l = enabled();
        start_periodic(&mut l, 625_000, 0);
        // Three periods go by unobserved.
        l.poll(35_000 * US);
        l.acknowledge(32);
        l.eoi();
        assert_eq!(l.pending(), None, "coalesced into one request");
        // The next expiry is the 40 ms boundary, not 35 + 10.
        assert_eq!(l.next_event(), Some(40_000 * US));
    }

    #[test]
    fn one_shot_fires_once_then_reads_zero() {
        let mut l = enabled();
        l.write(0x3E0, 0b0011, 0);
        l.write(0x320, 33, 0); // one-shot, vector 33
        l.write(0x380, 62_500, 0); // 1 ms
        l.poll(999 * US);
        assert_eq!(l.pending(), None);
        l.poll(1_000 * US);
        assert_eq!(l.pending(), Some(33));
        assert_eq!(l.next_event(), None);
        assert_eq!(l.read(0x390, 5_000 * US), 0);
        // The initial count register keeps what was written.
        assert_eq!(l.read(0x380, 5_000 * US), 62_500);
    }

    #[test]
    fn current_count_counts_down_and_reloads() {
        let mut l = enabled();
        start_periodic(&mut l, 625_000, 0);
        assert_eq!(l.read(0x390, 0), 625_000);
        // Half way: 5 ms at /16 of 1 GHz is 312 500 counts.
        assert_eq!(l.read(0x390, 5_000 * US), 312_500);
        // Just before expiry the count is small and non-zero.
        assert_eq!(l.read(0x390, 10_000 * US - 16), 1);
        // After the reload it starts over.
        assert_eq!(l.read(0x390, 10_000 * US), 625_000);
        assert_eq!(l.read(0x390, 12_500 * US), 625_000 - 156_250);
    }

    #[test]
    fn calibration_the_way_akumas_driver_does_it() {
        // `calibrate`: divide 16, masked, init = u32::MAX, read CUR after ~10 ms
        // of PIT time, elapsed = MAX - remaining.
        let mut l = enabled();
        l.write(0x3E0, 0b0011, 0);
        l.write(0x320, LVT_MASKED, 0);
        l.write(0x380, u32::MAX, 0);
        let remaining = l.read(0x390, 10_000 * US);
        let elapsed = u32::MAX - remaining;
        assert_eq!(elapsed, 625_000);
        // Writing 0 stops it and nothing was raised (it was masked).
        l.write(0x380, 0, 10_000 * US);
        assert_eq!(l.read(0x390, 11_000 * US), 0);
        assert_eq!(l.pending(), None);
    }

    #[test]
    fn a_masked_timer_counts_but_raises_nothing() {
        let mut l = enabled();
        l.write(0x3E0, 0b0011, 0);
        l.write(0x320, (1 << 17) | LVT_MASKED | 32, 0);
        l.write(0x380, 62_500, 0);
        l.poll(10_000 * US);
        assert_eq!(l.pending(), None);
        assert_eq!(l.next_event(), None, "nothing a halted guest could wake for");
    }

    #[test]
    fn rewriting_the_initial_count_restarts_the_period() {
        let mut l = enabled();
        start_periodic(&mut l, 625_000, 0);
        // Akuma's `arm_deadline`: LVT to one-shot, then a shorter count.
        l.write(0x320, 32, 4_000 * US);
        l.write(0x380, 62_500, 4_000 * US);
        assert_eq!(l.next_event(), Some(5_000 * US));
    }

    #[test]
    fn changing_the_divisor_keeps_the_remaining_count() {
        let mut l = enabled();
        start_periodic(&mut l, 1_000, 0);
        // /16 of 1 GHz: 1000 counts take 16 us. Halfway, 500 remain; at /32
        // they take 16 us more.
        l.write(0x3E0, 0b1000, 8 * US);
        assert_eq!(l.next_event(), Some(8 * US + 500 * 32));
    }

    #[test]
    fn priority_orders_the_offer_and_the_in_service_vector_blocks_its_class() {
        let mut l = enabled();
        l.raise(0x31);
        l.raise(0x62);
        assert_eq!(l.pending(), Some(0x62));
        l.acknowledge(0x62);
        // 0x31 is a lower class than the in-service 0x62: held back.
        assert_eq!(l.pending(), None);
        assert_eq!(l.ppr(), 0x60);
        // A higher class gets through (nesting).
        l.raise(0x80);
        assert_eq!(l.pending(), Some(0x80));
        l.acknowledge(0x80);
        assert_eq!(l.eoi(), Some(0x80));
        assert_eq!(l.eoi(), Some(0x62));
        assert_eq!(l.pending(), Some(0x31));
    }

    #[test]
    fn the_same_class_does_not_nest() {
        let mut l = enabled();
        l.raise(0x61);
        l.acknowledge(0x61);
        l.raise(0x6F);
        assert_eq!(l.pending(), None, "0x6F is the same class as the in-service 0x61");
        l.eoi();
        assert_eq!(l.pending(), Some(0x6F));
    }

    #[test]
    fn the_task_priority_masks_its_class_and_below() {
        let mut l = enabled();
        l.write(0x80, 0x50, 0);
        l.raise(0x4F);
        l.raise(0x55);
        assert_eq!(l.pending(), None);
        l.raise(0x60);
        assert_eq!(l.pending(), Some(0x60));
        assert_eq!(l.read(0xA0, 0), 0x50, "PPR is the TPR while nothing is in service");
    }

    #[test]
    fn isr_and_irr_read_back_per_32_vectors() {
        let mut l = enabled();
        l.raise(0x20); // word 1, bit 0
        l.raise(0xFF); // word 7, bit 31
        assert_eq!(l.read(0x210, 0), 1);
        assert_eq!(l.read(0x270, 0), 1 << 31);
        l.acknowledge(0xFF);
        assert_eq!(l.read(0x170, 0), 1 << 31);
        assert_eq!(l.read(0x270, 0), 0);
        // TMR is level-trigger state: all edge here.
        assert_eq!(l.read(0x1F0, 0), 0);
    }

    #[test]
    fn an_eoi_with_nothing_in_service_is_harmless() {
        let mut l = enabled();
        l.write(0xB0, 0, 0);
        assert_eq!(l.pending(), None);
    }

    #[test]
    fn illegal_vectors_set_the_error_register_and_are_dropped() {
        let mut l = enabled();
        l.raise(0x0F);
        assert_eq!(l.pending(), None);
        assert_eq!(l.read(0x280, 0), 1 << 6);
        l.write(0x280, 0, 0);
        assert_eq!(l.read(0x280, 0), 0);
    }

    #[test]
    fn a_self_ipi_is_delivered_and_others_are_dropped() {
        let mut l = enabled();
        // Shorthand "self" (bits 19:18 = 01), fixed, vector 0x44.
        l.write(0x300, (0b01 << 18) | 0x44, 0);
        assert_eq!(l.pending(), Some(0x44));
        l.acknowledge(0x44);
        l.eoi();
        // Shorthand "all excluding self": no other processor exists.
        l.write(0x300, (0b11 << 18) | 0x45, 0);
        assert_eq!(l.pending(), None);
        // No shorthand: the destination field decides.
        l.write(0x310, 7 << 24, 0);
        l.write(0x300, 0x46, 0);
        assert_eq!(l.pending(), None);
        l.write(0x310, 0, 0);
        l.write(0x300, 0x46, 0);
        assert_eq!(l.pending(), Some(0x46));
        // INIT to a processor that is not this one raises nothing here.
        l.acknowledge(0x46);
        l.eoi();
        l.write(0x300, (0b101 << 8) | 0xC000, 0);
        assert_eq!(l.pending(), None);
    }

    #[test]
    fn other_processors_requests_wait_in_the_outbox() {
        let mut l = enabled().with_id(1);
        // Fixed, physical destination 2: nothing for the sender, one request out.
        l.write(0x310, 2 << 24, 0);
        l.write(0x300, 0x50, 0);
        assert_eq!(l.pending(), None);
        let ipi = l.take_ipi().expect("routed");
        assert_eq!((ipi.mode, ipi.vector, ipi.dest), (IpiMode::Fixed, 0x50, IpiDest::Id(2)));
        assert!(ipi.reaches(2) && !ipi.reaches(0));
        assert_eq!(l.take_ipi(), None, "taken once");
        // "All including self": the sender takes its copy, the rest are routed.
        l.write(0x300, (0b10 << 18) | 0x51, 0);
        assert_eq!(l.pending(), Some(0x51));
        assert_eq!(l.take_ipi().map(|i| i.dest), Some(IpiDest::All));
        // "All excluding self" is not delivered here.
        l.acknowledge(0x51);
        l.eoi();
        l.write(0x300, (0b11 << 18) | 0x52, 0);
        assert_eq!(l.pending(), None);
        assert_eq!(l.take_ipi().map(|i| i.dest), Some(IpiDest::AllButSelf));
        // An IPI to itself by id has nobody to route to.
        l.write(0x310, 1 << 24, 0);
        l.write(0x300, 0x53, 0);
        assert_eq!(l.pending(), Some(0x53));
        assert_eq!(l.take_ipi(), None);
    }

    #[test]
    fn init_and_startup_are_routed_and_an_init_deassert_is_not() {
        let mut l = enabled();
        l.write(0x310, 1 << 24, 0);
        // INIT, level-triggered assert (the Akuma guest's `send_init` shape).
        l.write(0x300, (0b101 << 8) | (1 << 14) | (1 << 15), 0);
        assert_eq!(l.take_ipi().map(|i| i.mode), Some(IpiMode::Init));
        // INIT level de-assert: dropped.
        l.write(0x300, (0b101 << 8) | (1 << 15), 0);
        assert_eq!(l.take_ipi(), None);
        // STARTUP, vector 0x08: the page the guest's trampoline sits in.
        l.write(0x300, (0b110 << 8) | 0x08, 0);
        let sipi = l.take_ipi().expect("routed");
        assert_eq!((sipi.mode, sipi.vector, sipi.dest), (IpiMode::Startup, 0x08, IpiDest::Id(1)));
        // Lowest-priority, NMI, SMI, reserved: not modelled.
        for mode in [0b001u32, 0b010, 0b100, 0b111] {
            l.write(0x300, mode << 8, 0);
            assert_eq!(l.take_ipi(), None, "mode {mode:#b}");
        }
    }

    #[test]
    fn init_returns_the_unit_to_reset_but_keeps_its_id() {
        let mut l = enabled().with_id(3);
        start_periodic(&mut l, 625_000, 0);
        l.raise(0x40);
        l.init_reset();
        assert_eq!(l.id(), 3);
        assert!(!l.enabled());
        assert_eq!(l.next_event(), None);
        l.write(0xF0, 0x1FF, 0);
        assert_eq!(l.pending(), None, "requests were cleared");
    }

    #[test]
    fn a_pending_request_survives_a_disable_and_reenable() {
        let mut l = enabled();
        l.raise(0x40);
        l.write(0xF0, 0xFF, 0);
        assert_eq!(l.pending(), None);
        l.write(0xF0, 0x1FF, 0);
        assert_eq!(l.pending(), Some(0x40));
    }

    #[test]
    fn expiry_before_a_reconfiguring_write_is_charged_to_the_old_configuration() {
        let mut l = enabled();
        start_periodic(&mut l, 625_000, 0);
        // The 10 ms edge passes; the guest then masks the entry. The request
        // was already due and stays raised.
        l.write(0x320, LVT_MASKED | 32 | (1 << 17), 10_500 * US);
        assert_eq!(l.pending(), Some(32));
    }

    #[test]
    fn lint0_extint_is_the_virtual_wire() {
        let mut l = Lapic::new();
        // Reset: masked, and the unit is disabled.
        assert!(!l.lint0_extint());
        l.write(0xF0, 0x1FF, 0);
        assert!(!l.lint0_extint(), "still masked");
        // ExtINT (delivery mode 111), unmasked.
        l.write(0x350, 0x700, 0);
        assert!(l.lint0_extint());
        // Fixed delivery on vector 0x30 is not a virtual wire.
        l.write(0x350, 0x30, 0);
        assert!(!l.lint0_extint());
        // Masked ExtINT is not either, and neither is a disabled unit.
        l.write(0x350, 0x700 | LVT_MASKED, 0);
        assert!(!l.lint0_extint());
        l.write(0x350, 0x700, 0);
        l.write(0xF0, 0xFF, 0);
        assert!(!l.lint0_extint());
    }

    #[test]
    fn bus_ticks_are_nanoseconds() {
        assert_eq!(bus_ticks_from_us(1), 1_000);
        assert_eq!(bus_ticks_from_us(10_000), 10_000_000);
    }
}
