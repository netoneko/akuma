//! The LAPIC's timer face and the clocks measured against it, extracted from
//! `amd64/src/lapic.rs` per the composition rule ("extract into a crate only
//! what lives only there"): the register-page window, end-of-interrupt, the
//! tick counter, the periodic timer arm/stop, the 8254 PIT calibration, and
//! the TSC rate/uptime math — **one implementation, two consumers**. The
//! amd64 kernel (`amd64/src/lapic.rs`) re-exports these and keeps the parts
//! that need its own modules (IPI/ICR, per-core oneshot bookkeeping, AP
//! bring-up, self-tests); taoxichuan consumes it directly. The KVM guest's
//! LAPIC model (`akuma-svm-amd64`'s `lapic`, xAPIC page + timer) is what both
//! kernels run against.
//!
//! `#![forbid(unsafe_code)]`, budget 0: every hardware access rides a safe
//! abstraction that already existed — [`akuma_primitives::mmio::MmioWindow`]
//! (the caller maps the page, so the unsafe of *mapping* stays in the
//! consumer), [`akuma_port::PIT`] (the only crate that does `in`/`out`), and
//! [`akuma_cpu::tsc::rdtsc`]. The one unavoidable `unsafe` of the old file —
//! the `IA32_APIC_BASE` global-enable write — stays with the caller, who
//! passes the already-written MSR value in; see [`init`].
//!
//! What this crate deliberately does **not** have: IPIs (`REG_ICR`), the
//! spurious-vector/`init_ap` SMP path, per-core oneshot/idle-shot tables and
//! TSC-sync offsets (all `crate::smp`-shaped in the amd64 kernel), and the
//! scheduler calls [`amd64`'s `on_tick`] makes around the count. A consumer
//! whose tick needs more wraps [`on_tick`]; nobody here is preempted yet.

#![no_std]
#![forbid(unsafe_code)]

use akuma_cpu::tsc::rdtsc;
use akuma_port::PIT;
use akuma_primitives::{mmio::MmioWindow, OnceCopy};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

// ── constants (verbatim from amd64/src/lapic.rs) ────────────────────────────

/// The IA32_APIC_BASE fields this crate reads: the page's physical base.
pub const APIC_BASE_MASK: u64 = 0x000f_ffff_ffff_f000;
/// The IA32_APIC_BASE global-enable bit. The caller writes it (`unsafe`); this
/// crate only names it so both kernels agree.
pub const APIC_GLOBAL_ENABLE: u64 = 1 << 11;

/// The spurious-delivery vector parked on in [`software_enable`]. Distinguishable
/// from a real vector; nothing handles it here.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// The timer's IDT vector — a software choice fixed here so the LVT write in
/// [`start_timer`] and the consumer's IDT entry cannot disagree.
pub const TIMER_VECTOR: u8 = 32;

/// Target period of one tick, in microseconds. `net::uptime_us` on both
/// kernels multiplies [`ticks`] by this.
pub const US_PER_TICK_TARGET: u32 = 10_000;

/// The initial count when calibration could not run: the historical value, kept
/// so a machine with no PIT behaves exactly as it did before calibration
/// existed rather than differently-wrongly.
const UNCALIBRATED_COUNT: u32 = 100_000;

/// The PIT's input frequency, in Hz. Fixed by the hardware.
const PIT_HZ: u32 = 1_193_182;
/// The longest interval a 16-bit PIT channel can express, in microseconds.
const PIT_MAX_US: u32 = 54_000;

// Register offsets into the xAPIC page (dword units in the MMIO window).
const REG_ID: usize = 0x020;
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CUR: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;

const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const LVT_MASKED: u32 = 1 << 16;
const TIMER_DIV_16: u32 = 0b0011;

// ── state (moved verbatim) ──────────────────────────────────────────────────

/// Ticks seen so far. The BSP's count is the kernel's clock.
static TICKS: AtomicU64 = AtomicU64::new(0);
/// The xAPIC register page, mapped by the caller and handed to [`init`].
static LAPIC: OnceCopy<MmioWindow> = OnceCopy::new();
/// Backs [`timer_running`]. Set by [`start_timer`], cleared by [`stop_timer`].
static TIMER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Counts to load for one [`US_PER_TICK_TARGET`] period. Written once by
/// [`calibrate`], read by every [`start_timer`].
static TIMER_COUNT: AtomicU32 = AtomicU32::new(UNCALIBRATED_COUNT);
/// Whether [`calibrate`] found a PIT and succeeded.
static CALIBRATED: AtomicBool = AtomicBool::new(false);
/// The TSC rate in Hz, measured across the same PIT gate as the LAPIC count;
/// 0 until [`calibrate`] succeeds. Read by [`tsc_hz`].
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// The TSC value [`calibrate`] read as its `tsc0` — boot's zero point for
/// [`tsc_uptime_us`]. Meaningless while [`TSC_HZ`] is still 0.
static TSC_START: AtomicU64 = AtomicU64::new(0);

// ── register access (the old `window`/`read`/`write`, safe by construction) ─

fn window() -> MmioWindow {
    LAPIC.get().expect("LAPIC register access before akuma_lapic_amd64::init was handed the page")
}

/// Read one xAPIC register. Pub for the amd64 kernel's ICR/`send_ipi` path —
/// the one LAPIC face this crate does not own.
#[must_use]
pub fn read_reg(offset: usize) -> u32 {
    window().read::<u32>(offset)
}

/// Write one xAPIC register. Pub for the amd64 kernel's ICR/`send_ipi` path.
pub fn write_reg(offset: usize, val: u32) {
    window().write::<u32>(offset, val);
}

/// Has [`init`] been handed this core's (the guest's) LAPIC page?
#[must_use]
pub fn ready() -> bool {
    LAPIC.is_set()
}

/// This core's APIC id, from its own ID register.
#[must_use]
pub fn apic_id() -> u32 {
    read_reg(REG_ID) >> 24
}

/// Software-enable the LAPIC and park spurious deliveries on `spurious_vector`.
/// The amd64 kernel calls this from `init_ap` (the per-core SMP path); taoxichuan
/// reaches it through [`init`].
pub fn software_enable(spurious_vector: u8) {
    write_reg(REG_SVR, (1 << 8) | u32::from(spurious_vector));
}

// ── the tick ────────────────────────────────────────────────────────────────

/// Signal end-of-interrupt. Every handler for a LAPIC-delivered vector must do
/// this before returning, or the LAPIC will not deliver that priority again.
pub fn eoi() {
    write_reg(REG_EOI, 0);
}

/// Ticks seen so far.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Count one tick on the BSP clock and return the new count. The amd64
/// kernel's `on_tick` calls this from its per-core arm; single-core consumers
/// use [`on_tick`].
pub fn advance_tick() -> u64 {
    TICKS.fetch_add(1, Ordering::Relaxed) + 1
}

/// `init` was handed an `IA32_APIC_BASE` value with no base address in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoApicBase;

/// The minimal timer-vector body: count one tick and acknowledge it.
///
/// Whatever else a tick does — reschedule requests, watchdog pets, one-shot credits —
/// belongs to the consumer's wrapper, which is why this returns the new count.
#[must_use]
pub fn on_tick() -> u64 {
    let t = advance_tick();
    eoi();
    t
}

// ── the periodic timer ──────────────────────────────────────────────────────

/// Arm the periodic timer.
///
/// The count comes from [`calibrate`] when a PIT was there to calibrate
/// against, and from [`UNCALIBRATED_COUNT`] when there was not. (The error
/// story — `100_000` at divide-16 being 6x fast on a 1 GHz KVM APIC and 1.6x
/// slow on a 100 MHz bus — is told at length in `amd64/src/lapic.rs`'s
/// `start_timer`, whose body this is.)
pub fn start_timer() {
    write_reg(REG_TIMER_DIV, TIMER_DIV_16);
    write_reg(REG_LVT_TIMER, u32::from(TIMER_VECTOR) | LVT_TIMER_PERIODIC);
    write_reg(REG_TIMER_INIT, TIMER_COUNT.load(Ordering::Relaxed));
    TIMER_RUNNING.store(true, Ordering::Relaxed);
}

/// Stop the timer (mask the LVT, zero the count). Used after self-tests so
/// later output is not interleaved with ticks.
pub fn stop_timer() {
    write_reg(REG_LVT_TIMER, LVT_MASKED);
    write_reg(REG_TIMER_INIT, 0);
    TIMER_RUNNING.store(false, Ordering::Relaxed);
}

/// Is the periodic tick armed?
#[must_use]
pub fn timer_running() -> bool {
    TIMER_RUNNING.load(Ordering::Relaxed)
}

/// Borrow the timer as a masked full-scale one-shot and spin until it expires:
/// the busy-delay primitive (`delay`), moved as it was.
pub fn delay_counts(counts: u32) {
    write_reg(REG_TIMER_DIV, TIMER_DIV_16);
    write_reg(REG_LVT_TIMER, LVT_MASKED);
    write_reg(REG_TIMER_INIT, counts);
    while read_reg(REG_TIMER_CUR) != 0 {
        core::hint::spin_loop();
    }
    write_reg(REG_TIMER_INIT, 0);
}

/// The timer's current count — the one-shot bookkeeping the amd64 kernel does
/// per core ("does the periodic tick serve the deadline as soon?") reads this.
#[must_use]
pub fn timer_current_count() -> u32 {
    read_reg(REG_TIMER_CUR)
}

/// Switch this core's LVT timer to unmasked one-shot mode with `counts` as the
/// initial count. The amd64 oneshot/idle-shot machinery calls this; the
/// periodic tick comes back through [`start_timer`].
pub fn arm_oneshot(counts: u32) {
    write_reg(REG_LVT_TIMER, u32::from(TIMER_VECTOR));
    write_reg(REG_TIMER_INIT, counts);
}

/// Credit `extra` ticks that never fired (an idle long shot replacing them),
/// returning the new count.
pub fn credit_ticks(extra: u64) -> u64 {
    TICKS.fetch_add(extra, Ordering::Relaxed) + extra
}

// ── calibration against the 8254 PIT (moved verbatim) ───────────────────────

/// Did [`calibrate`] succeed? When false the tick period is whatever the core
/// crystal makes of [`UNCALIBRATED_COUNT`] and `uptime_us` is a guess.
#[must_use]
pub fn is_calibrated() -> bool {
    CALIBRATED.load(Ordering::Relaxed)
}

/// LAPIC counts measured in one [`US_PER_TICK_TARGET`] period, or 0.
#[must_use]
pub fn calibrated_count() -> u32 {
    if CALIBRATED.load(Ordering::Relaxed) {
        TIMER_COUNT.load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Measure the LAPIC timer against the 8254 PIT and set [`TIMER_COUNT`] so one
/// tick is [`US_PER_TICK_TARGET`]. Returns whether it succeeded.
///
/// The full story — channel 0 not channel 2 (no port-0x61 board glue, so it
/// works on QEMU `microvm`), the read-back command instead of an IRQ, the
/// bounded wait for machines with no PIT at all, and the plausibility bands —
/// is in `amd64/src/lapic.rs`'s `calibrate`, whose body this is. The TSC rides
/// along on the same load: one PIT period measures both clocks.
pub fn calibrate() -> bool {
    /// Ticks of the PIT in one target period.
    const PIT_COUNT: u32 = (PIT_HZ as u64 * US_PER_TICK_TARGET as u64 / 1_000_000) as u32;
    /// Spins before giving up on an output that is never going to latch high.
    const SPIN_LIMIT: u32 = 50_000_000;

    // Nothing else programs PIT channel 0 — the legacy PICs are masked before
    // `calibrate` runs (the caller's `init` ordering), so even if this makes it
    // raise IRQ0 nothing delivers it. Mode 0, and the load itself starts the
    // countdown.
    PIT.load_channel0_oneshot(PIT_COUNT as u16);

    // Arm the LAPIC at full scale, masked — this borrows the timer exactly as
    // `delay_counts` does, and `start_timer` re-arms it afterwards.
    write_reg(REG_TIMER_DIV, TIMER_DIV_16);
    write_reg(REG_LVT_TIMER, LVT_MASKED);
    write_reg(REG_TIMER_INIT, u32::MAX);
    let tsc0 = rdtsc();

    // A freshly loaded mode-0 count holds OUTPUT low until it expires. If it
    // reads high immediately, this is not a real PIT answering (an unimplemented
    // port reads 0xFF) — the belt-and-braces against the 50x-clock failure.
    if PIT.channel0_output_high() {
        write_reg(REG_TIMER_INIT, 0);
        return false;
    }

    let mut spins: u32 = 0;
    while !PIT.channel0_output_high() {
        spins += 1;
        if spins >= SPIN_LIMIT {
            write_reg(REG_TIMER_INIT, 0);
            return false;
        }
        core::hint::spin_loop();
    }
    let remaining = read_reg(REG_TIMER_CUR);
    let tsc1 = rdtsc();
    write_reg(REG_TIMER_INIT, 0);

    let elapsed = u32::MAX - remaining;
    // A plausibility band rather than a bare non-zero check.
    if !(1_000..=500_000_000).contains(&elapsed) {
        return false;
    }
    TIMER_COUNT.store(elapsed, Ordering::Relaxed);
    let tsc_ticks = tsc1.wrapping_sub(tsc0);
    let hz = tsc_ticks * 1_000_000 / u64::from(US_PER_TICK_TARGET);
    if hz >= 100_000_000 {
        // `tsc0`, not `tsc1`: boot's zero point is the instant the measurement
        // started, so `tsc_uptime_us` agrees with `ticks()` about "just now".
        TSC_START.store(tsc0, Ordering::Relaxed);
        TSC_HZ.store(hz, Ordering::Relaxed);
    }
    CALIBRATED.store(true, Ordering::Relaxed);
    true
}

/// Run PIT channel 0 for `us` microseconds and spin until it expires.
///
/// The measurement primitive the amd64 kernel's `clock_rate_check` self-test uses.
/// `us` must be at most ~54.9 ms (the 16-bit channel's longest interval).
#[must_use]
pub fn pit_wait_us(us: u32) -> bool {
    const SPIN_LIMIT: u32 = 500_000_000;

    if us == 0 || us > PIT_MAX_US {
        return false;
    }
    let count = (u64::from(PIT_HZ) * u64::from(us) / 1_000_000) as u32;
    if count == 0 || count > 0xFFFF {
        return false;
    }
    PIT.load_channel0_oneshot(count as u16);

    if PIT.channel0_output_high() {
        return false;
    }
    let mut spins: u32 = 0;
    while !PIT.channel0_output_high() {
        spins += 1;
        if spins >= SPIN_LIMIT {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

// ── the TSC clock (moved; single-core form) ─────────────────────────────────

/// The TSC frequency in Hz, or 0 when no PIT was there to measure it against.
/// A caller that gets 0 must pick a conservative rate itself; this function
/// does not guess.
#[must_use]
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Monotonic microseconds since boot, at TSC resolution — `None` until
/// [`calibrate`] has run and found a real rate.
///
/// Single-core form: the per-core TSC-offset correction the amd64 kernel
/// applies once APs are up (`TSC_OFFSETS_LIVE`) is absent here, which is
/// behaviour-identical until the SMP slice exists (the offsets are all zero /
/// the flag false on a BSP-only kernel). `u128` for the multiply, as in the
/// original: a delta more than ~213 days old would overflow a `u64`.
#[must_use]
pub fn tsc_uptime_us() -> Option<u64> {
    let hz = tsc_hz();
    if hz == 0 {
        return None;
    }
    let now = rdtsc();
    let delta = now.wrapping_sub(TSC_START.load(Ordering::Relaxed));
    // A reading taken just after boot on a TSC a hair behind `TSC_START` would
    // wrap to ~2^64; clamp instead.
    if delta > u64::MAX / 2 {
        return Some(0);
    }
    Some((u128::from(delta) * 1_000_000 / u128::from(hz)) as u64)
}

// ── init ────────────────────────────────────────────────────────────────────

/// What [`init`] set up, for the caller's console line.
#[derive(Clone, Copy)]
pub struct TimerInfo {
    /// The LAPIC page's physical base, decoded from the MSR value passed in.
    pub base: u64,
    /// This core's APIC id.
    pub id: u32,
    /// Whether [`calibrate`] found a PIT.
    pub calibrated: bool,
    /// LAPIC counts per [`US_PER_TICK_TARGET`] when calibrated.
    pub timer_count: u32,
    /// The measured TSC rate in Hz, 0 if not measured.
    pub tsc_hz: u64,
}

/// Bring up the timer face of an already-enabled LAPIC.
///
/// The caller's half (the half that needs `unsafe` or a kernel module): mask
/// the legacy PICs, set `IA32_APIC_BASE.APIC_GLOBAL_ENABLE` if firmware left
/// it clear, map the 4 KiB xAPIC page device-uncached, and register the
/// `TIMER_VECTOR` IDT entry. This half: name the base, software-enable the
/// LAPIC with a spurious vector of its own, calibrate against the PIT, and
/// arm the periodic timer. Interrupts stay masked; the caller decides when
/// to `sti`.
pub fn init(base_msr_enabled: u64, win: MmioWindow) -> Result<TimerInfo, NoApicBase> {
    let base = base_msr_enabled & APIC_BASE_MASK;
    if base == 0 {
        return Err(NoApicBase);
    }
    LAPIC.set(win);
    software_enable(SPURIOUS_VECTOR);
    // Before the timer is armed for real: `calibrate` borrows it (masked,
    // one-shot, full scale) and `start_timer` re-arms it with whatever count
    // came out.
    let calibrated = calibrate();
    start_timer();
    Ok(TimerInfo {
        base,
        id: apic_id(),
        calibrated,
        timer_count: TIMER_COUNT.load(Ordering::Relaxed),
        tsc_hz: tsc_hz(),
    })
}
