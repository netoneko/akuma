//! ARM generic timer hardware, kernel timekeeping, and the self-tuning
//! scheduler-tick policy.
//!
//! This is the hardware/RTC half of the old `src/timer.rs`, extracted per
//! `docs/archive/TRIM_FAT_EMBARRESSING_DUPLICATIONS.md` § "Deferred audit":
//! the scheduler-ISR half (SGI kick, preemption watchdog, itimer delivery,
//! `kernel_timer` alarm queue) stays in the bin crate — it is fused to
//! `akuma_exec` — and registers itself on top of this crate's periodic tick.
//!
//! # Self-tuning tick
//!
//! The tick interval is chosen at boot by measuring whether the host actually
//! honours WFI at each candidate interval (`policy::pick_tick`): under some
//! hypervisors (QEMU HVF on darwin/arm64) the host declines to sleep vCPU
//! threads for deadlines below ~2.5 ms, which turns WFI into a no-op and the
//! idle loops into busy-polls burning one host core per guest core
//! (`docs/archive/CPU_LOAD_REGRESSION_INVESTIGATION.md`). A runtime governor
//! (`policy::governor_observe`) demotes the tick if the host's behaviour
//! changes after boot.
//!
//! # Host testability
//!
//! Policy is pure over a mocked hardware seam ([`Hw`]); the real
//! register access ([`ArchHw`]) exists only on `target_os = "none"`.

#![no_std]

#[cfg(target_os = "none")]
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

// ============================================================================
// Hardware seam
// ============================================================================

/// The virtual-timer hardware operations the tick policy needs.
///
/// A trait (rather than direct register access) so host tests can script WFI
/// behaviour — the whole point of the probe is distinguishing a host that
/// sleeps from one that returns instantly.
pub trait Hw {
    /// Current CNTVCT value in timer ticks.
    fn counter_ticks(&self) -> u64;
    /// CNTFRQ in Hz (ticks per second).
    fn frequency_hz(&self) -> u64;
    /// Arm the virtual timer to fire (one-shot) at absolute tick `deadline`,
    /// unmasked.
    fn arm_oneshot_ticks(&self, deadline: u64);
    /// Disarm the virtual timer (mask it).
    fn disarm(&self);
    /// Halt until an interrupt is pending.
    fn wfi(&self);
}

/// Conversion helpers shared by the policy and the bin crate's ISR half.
#[must_use]
#[inline]
pub fn ticks_from_us(freq_hz: u64, us: u64) -> u64 {
    ((u128::from(freq_hz) * u128::from(us)) / 1_000_000u128) as u64
}

/// Real CNTV access. Only meaningful on the no_std target; host builds (tests,
/// clippy) never construct it.
#[cfg(target_os = "none")]
pub struct ArchHw;

#[cfg(target_os = "none")]
impl Hw for ArchHw {
    fn counter_ticks(&self) -> u64 {
        read_counter()
    }

    fn frequency_hz(&self) -> u64 {
        read_frequency()
    }

    fn arm_oneshot_ticks(&self, deadline: u64) {
        unsafe {
            asm!("msr cntv_cval_el0, {}", in(reg) deadline);
            // bit 0 = enable, bit 1 = !mask
            asm!("msr cntv_ctl_el0, {}", in(reg) 1u64);
        }
    }

    fn disarm(&self) {
        unsafe {
            asm!("msr cntv_ctl_el0, {}", in(reg) 0u64);
        }
    }

    fn wfi(&self) {
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

// ============================================================================
// Tick interval registry
// ============================================================================

/// The current scheduler tick in microseconds. `0` = not yet set; the bin
/// crate sets it at boot (probe result, override, or compiled default) and the
/// ISR re-arms from it every interrupt, so a runtime demotion takes effect on
/// the very next tick without any cross-core broadcast.
static TICK_US: AtomicU64 = AtomicU64::new(0);

/// Set the scheduler tick (called at boot and by governor demotion).
#[inline]
pub fn set_tick_us(us: u64) {
    TICK_US.store(us, Ordering::Relaxed);
}

/// The current scheduler tick, or `fallback_us` if none was set yet.
#[inline]
pub fn tick_us(fallback_us: u64) -> u64 {
    let t = TICK_US.load(Ordering::Relaxed);
    if t > 0 {
        t
    } else {
        fallback_us
    }
}

/// Arm the timer for one periodic tick from now (enable path). The ISR half in
/// the bin crate re-arms from the *entry* counter value instead, so a slow
/// handler does not shorten the next period.
#[cfg(target_os = "none")]
#[inline]
pub fn arm_periodic_tick() {
    let interval = ticks_from_us(read_frequency(), tick_us(10_000));
    let deadline = read_counter() + interval;
    unsafe {
        asm!("msr cntv_cval_el0, {}", in(reg) deadline);
        asm!("msr cntv_ctl_el0, {}", in(reg) 1u64);
    }
}

/// Disarm the virtual timer (mask it).
///
/// Also de-asserts a pending level: the timer condition stays true while
/// `CVAL <= counter` and enabled, so an unmasked IRQ left in that state
/// re-forwards forever after EOI. This is why the probe's NOP handler calls
/// this instead of doing nothing.
#[cfg(target_os = "none")]
#[inline]
pub fn disarm() {
    unsafe {
        asm!("msr cntv_ctl_el0, {}", in(reg) 0u64);
    }
}

// ============================================================================
// Tick policy (pure, host-testable)
// ============================================================================

pub mod policy {
    use super::{Hw, ticks_from_us};

    /// Candidate tick intervals probed at boot, ascending; the smallest whose
    /// WFI the host honours wins. Chosen against the measured HVF cliff
    /// (100%/100% at 1–2 ms vs 1.6% at 3 ms, SMP=1, 2026-08-18).
    pub const PROBE_CANDIDATES_US: &[u64] = &[1_000, 2_000, 3_000, 5_000];

    /// Ultimate fallback if the host honours WFI at no candidate (pathological
    /// — every sample returns instantly even at 5 ms): the historical 10 ms
    /// tick, which is safe everywhere.
    pub const FALLBACK_US: u64 = 10_000;

    /// Samples per candidate. A single fast sample is expected even on a good
    /// host (an unrelated IRQ wins the race), so the criterion is a fraction,
    /// not a minimum.
    const SAMPLES: usize = 8;

    /// Samples that must show a real halt (>= interval/2) for a candidate to
    /// pass.
    const PASS_FRAC: usize = 6;

    /// Demotion floor: once the governor trips, the tick lands here.
    pub const DEMOTE_TO_US: u64 = 5_000;

    /// An idle-loop iteration count per tick above this multiple is a spinning
    /// WFI (healthy is ~1 netpoll iteration per tick; a no-op WFI shows
    /// thousands — measured ~1.8M/s at a 1 ms tick on the regression host).
    const SPIN_ITER_PER_TICK: u64 = 10;

    /// Consecutive probe windows that must look spinny before demoting.
    const SPIN_WINDOWS: u32 = 2;

    /// Pick the scheduler tick by measuring whether WFI actually halts at
    /// each candidate interval.
    ///
    /// For each candidate (ascending): take [`SAMPLES`] one-shot WFI
    /// measurements; the candidate passes when at least [`PASS_FRAC`] show a
    /// halt of at least half the interval. Returns the first passing
    /// candidate, or [`FALLBACK_US`] if none pass. A `frequency_hz` of 0
    /// (unusable counter) also falls back — never divide by it, never trust a
    /// zero-duration measurement from a broken source.
    pub fn pick_tick(hw: &impl Hw) -> u64 {
        let freq = hw.frequency_hz();
        if freq == 0 {
            return FALLBACK_US;
        }
        for &cand_us in PROBE_CANDIDATES_US {
            let interval_ticks = ticks_from_us(freq, cand_us);
            let mut passed = 0;
            for _ in 0..SAMPLES {
                let t0 = hw.counter_ticks();
                hw.arm_oneshot_ticks(t0 + interval_ticks);
                hw.wfi();
                let elapsed_ticks = hw.counter_ticks().saturating_sub(t0);
                let elapsed_us = (u128::from(elapsed_ticks) * 1_000_000u128
                    / u128::from(freq)) as u64;
                if elapsed_us >= cand_us / 2 {
                    passed += 1;
                }
            }
            hw.disarm();
            if passed >= PASS_FRAC {
                return cand_us;
            }
        }
        FALLBACK_US
    }

    // ------------------------------------------------------------------------
    // Runtime governor
    // ------------------------------------------------------------------------

    /// State for the runtime demotion governor.
    ///
    /// Shared as one static from the timer ISR (per-core IRQ-masked context):
    /// races are benign — worst case two cores trip the demotion in the same
    /// window and both write the same value.
    #[must_use]
    pub struct Governor {
        spin_windows: u32,
        demoted: bool,
    }

    impl Governor {
        pub const fn new() -> Self {
            Self { spin_windows: 0, demoted: false }
        }

        /// Feed one observation window: `idle_iters` idle-loop iterations
        /// counted over `ticks` timer interrupts. Returns `Some(new_tick_us)`
        /// exactly once, when the host has stopped honouring WFI (the idle
        /// loop is spinning) for [`SPIN_WINDOWS`] consecutive windows.
        /// Never re-promotes — flipping back mid-flight buys a second burn.
        pub fn observe(&mut self, idle_iters: u64, ticks: u64) -> Option<u64> {
            if self.demoted || ticks == 0 {
                return None;
            }
            if idle_iters > ticks.saturating_mul(SPIN_ITER_PER_TICK) {
                self.spin_windows += 1;
            } else {
                self.spin_windows = 0;
            }
            if self.spin_windows >= SPIN_WINDOWS {
                self.demoted = true;
                return Some(DEMOTE_TO_US);
            }
            None
        }
    }

    impl Default for Governor {
        fn default() -> Self {
            Self::new()
        }
    }

    static GOVERNOR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    // GOVERNOR packs {spin_windows: bits 0..31, demoted: bit 32} so the whole
    // state stays one Relaxed atomic — see governor_observe.

    /// Static-wrapper convenience for the ISR: observe through the packed
    /// global governor state. Returns `Some(new_tick_us)` on demotion.
    pub fn governor_observe(idle_iters: u64, ticks: u64) -> Option<u64> {
        let mut g = Governor::new();
        let packed = GOVERNOR.load(core::sync::atomic::Ordering::Relaxed);
        g.spin_windows = (packed & 0xFFFF_FFFF) as u32;
        g.demoted = packed >> 32 != 0;
        let verdict = g.observe(idle_iters, ticks);
        GOVERNOR.store(
            u64::from(g.spin_windows) | (u64::from(g.demoted) << 32),
            core::sync::atomic::Ordering::Relaxed,
        );
        verdict
    }
}

// ============================================================================
// Timekeeping
// ============================================================================

/// Read the ARM virtual timer counter (CNTVCT).
///
/// Virtual, not physical (CNTPCT): the physical timer/counter is owned by the
/// hypervisor under QEMU HVF and trapping to it faults the guest (EC=0x0);
/// CNTVOFF is nonzero under HVF, so deadlines must use the same virtual base
/// the compare register runs on (`docs/archive/QEMU_HVF_ISV_BUG.md`).
#[cfg(target_os = "none")]
#[inline]
#[must_use]
pub fn read_counter() -> u64 {
    let counter: u64;
    unsafe {
        asm!("mrs {}, cntvct_el0", out(reg) counter);
    }
    counter
}

/// Read the timer frequency (CNTFRQ).
#[cfg(target_os = "none")]
#[inline]
#[must_use]
pub fn read_frequency() -> u64 {
    let freq: u64;
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    freq
}

/// Microseconds since boot (CNTVCT/CNTFRQ, u128 intermediate against
/// overflow). ~584-year horizon.
#[cfg(target_os = "none")]
#[inline]
#[must_use]
pub fn uptime_us() -> u64 {
    let counter = read_counter();
    let freq = read_frequency();
    if freq > 0 {
        ((u128::from(counter) * 1_000_000) / u128::from(freq)) as u64
    } else {
        0
    }
}

/// Host-build stub: the real `uptime_us` needs CNTVCT. Host builds (tests,
/// clippy) must not call it; the bin crate's re-export only exists on the
/// no_std target.
#[cfg(not(target_os = "none"))]
#[must_use]
#[inline]
pub fn uptime_us() -> u64 {
    0
}

// ============================================================================
// UTC offset + PL031 RTC
// ============================================================================

/// UTC offset in microseconds since the Unix epoch, or [`UTC_OFFSET_UNSET`].
///
/// A lock-free atomic rather than a `Spinlock<Option<u64>>`: the value is one
/// scalar with no other state published alongside it, and the read path is
/// reachable from a BKL-free syscall window (`futex(FUTEX_WAIT_BITSET|
/// CLOCK_REALTIME)` converts its absolute wall-clock deadline through this).
/// `UNSET` encodes the old `None` — a real offset is a Unix-epoch microsecond
/// count (~1.7e15), unreachable by four orders of magnitude.
const UTC_OFFSET_UNSET: u64 = u64::MAX;
static UTC_OFFSET_US: AtomicU64 = AtomicU64::new(UTC_OFFSET_UNSET);

/// Record the current instant as Unix epoch `unix_epoch_us`.
#[inline]
pub fn set_utc_time_us(unix_epoch_us: u64, boot_uptime_us: u64) {
    UTC_OFFSET_US.store(unix_epoch_us.saturating_sub(boot_uptime_us), Ordering::Release);
}

/// Current UTC in microseconds since the epoch, or `None` if never set.
#[inline]
pub fn utc_time_us(boot_uptime_us: u64) -> Option<u64> {
    match UTC_OFFSET_US.load(Ordering::Acquire) {
        UTC_OFFSET_UNSET => None,
        off => Some(off.wrapping_add(boot_uptime_us)),
    }
}

/// QEMU virt PL031 RTC at 0x0901_0000, reached via the kernel's fixed device
/// mapping. Only the raw seconds read lives here; presentation stays in the
/// bin crate.
///
/// SAFETY: the caller (bin crate, at boot) guarantees the MMU maps the PL031
/// window before `Rtc::new` is handed this address.
#[cfg(target_os = "none")]
pub mod rtc {
    use arm_pl031::Rtc;

    const PL031_BASE: *mut u32 = 0x0901_0000 as *mut _;

    /// Unix seconds from the PL031, or `None` if the RTC reads zero
    /// (unset/unpopulated battery-backed clock).
    pub fn unix_seconds() -> Option<u32> {
        // SAFETY: fixed QEMU virt PL031 address behind the kernel device
        // mapping (see module doc); a MMIO read only.
        let rtc = unsafe { Rtc::new(PL031_BASE) };
        let secs = rtc.get_unix_timestamp();
        (secs != 0).then_some(secs)
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::policy::{self, Governor, DEMOTE_TO_US, FALLBACK_US};
    use super::Hw;

    /// WFI halts exactly `wfi_us` (0 = the regression host: never sleeps),
    /// with every `fast_every`-th sample returning instantly (a racing
    /// unrelated IRQ).
    struct ScriptedHw {
        freq: u64,
        counter: Cell<u64>,
        wfi_us: u64,
        fast_every: usize,
        calls: Cell<usize>,
    }

    impl ScriptedHw {
        fn new(wfi_us: u64, fast_every: usize) -> Self {
            Self {
                freq: 64_000_000,
                counter: Cell::new(0),
                wfi_us,
                fast_every,
                calls: Cell::new(0),
            }
        }
    }

    impl Hw for ScriptedHw {
        fn counter_ticks(&self) -> u64 {
            self.counter.get()
        }

        fn frequency_hz(&self) -> u64 {
            self.freq
        }

        fn arm_oneshot_ticks(&self, _deadline: u64) {}

        fn disarm(&self) {}

        fn wfi(&self) {
            let n = self.calls.get();
            self.calls.set(n + 1);
            let halt_us = if self.fast_every != 0 && (n + 1) % self.fast_every == 0 {
                0
            } else {
                self.wfi_us
            };
            self.counter
                .set(self.counter.get() + halt_us * self.freq / 1_000_000);
        }
    }

    /// The regression host shape: below `floor_us` the "halt" returns
    /// instantly; at or above it, the full armed interval is slept.
    struct FlipHw {
        freq: u64,
        counter: Cell<u64>,
        floor_us: u64,
        armed_us: Cell<u64>,
    }

    impl Hw for FlipHw {
        fn counter_ticks(&self) -> u64 {
            self.counter.get()
        }

        fn frequency_hz(&self) -> u64 {
            self.freq
        }

        fn arm_oneshot_ticks(&self, deadline: u64) {
            let now = self.counter.get();
            self.armed_us.set(deadline.saturating_sub(now).max(1));
        }

        fn disarm(&self) {}

        fn wfi(&self) {
            let armed = self.armed_us.get();
            let elapsed = if armed < self.floor_us { 0 } else { armed };
            self.counter.set(self.counter.get() + elapsed * self.freq / 1_000_000);
        }
    }

    #[test]
    fn host_that_never_sleeps_falls_back() {
        let hw = ScriptedHw::new(0, 0);
        assert_eq!(policy::pick_tick(&hw), FALLBACK_US);
    }

    #[test]
    fn host_that_honours_1ms_gets_1ms() {
        let hw = ScriptedHw::new(1_000, 0);
        assert_eq!(policy::pick_tick(&hw), 1_000);
    }

    #[test]
    fn regression_host_picks_3ms() {
        // The 2026-08-18 measurements: instant return at 1 ms and 2 ms,
        // real sleep from 3 ms up (cliff between 2 and 3 ms, SMP=1).
        let hw = FlipHw {
            freq: 1_000_000,
            counter: Cell::new(0),
            floor_us: 2_500,
            armed_us: Cell::new(0),
        };
        assert_eq!(policy::pick_tick(&hw), 3_000);
    }

    #[test]
    fn racing_irq_does_not_veto_candidate() {
        // 2 fast samples out of 8 at an otherwise-honoured interval: still passes.
        let hw = ScriptedHw::new(1_000, 4);
        assert_eq!(policy::pick_tick(&hw), 1_000);
    }

    #[test]
    fn zero_frequency_falls_back() {
        struct ZeroFreqHw;
        impl Hw for ZeroFreqHw {
            fn counter_ticks(&self) -> u64 {
                0
            }
            fn frequency_hz(&self) -> u64 {
                0
            }
            fn arm_oneshot_ticks(&self, _d: u64) {}
            fn disarm(&self) {}
            fn wfi(&self) {}
        }
        assert_eq!(policy::pick_tick(&ZeroFreqHw), FALLBACK_US);
    }

    #[test]
    fn governor_demotes_after_two_spinny_windows_only() {
        let mut g = Governor::new();
        assert_eq!(g.observe(1_000_000, 2_000), None); // spinny window 1
        assert_eq!(g.observe(500, 2_000), None); // healthy: resets the run
        assert_eq!(g.observe(1_000_000, 2_000), None); // spinny 1
        assert_eq!(g.observe(1_000_000, 2_000), Some(DEMOTE_TO_US)); // spinny 2
        assert_eq!(g.observe(1_000_000, 2_000), None); // demoted: stays quiet
    }

    #[test]
    fn governor_ignores_zero_ticks() {
        let mut g = Governor::new();
        assert_eq!(g.observe(1_000_000, 0), None);
    }
}
