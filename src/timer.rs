// Timer ISR + presentation shim.
//
// The hardware half (CNTV access, PL031 RTC, UTC offset) and the self-tuning
// tick policy live in the `akuma-timer` crate (extracted 2026-08-18 per
// docs/archive/TRIM_FAT_EMBARRESSING_DUPLICATIONS.md's deferred-audit row:
// this file used to be scheduler-ISR logic wearing a driver's filename). What
// remains here is fused to the bin crate and cannot move:
//
// - `timer_irq_handler` — the ISR: re-arms the periodic tick, services the
//   `akuma_exec::alarms` queue, runs the preemption watchdog, feeds the
//   governor, and rings the scheduler SGI. All of that reaches akuma_exec.
// - `probe_host_tick` — boot-time WFI probe wiring (GIC + DAIF dance around
//   `akuma_timer::policy::pick_tick`).
// - UTC/ISO presentation (`DateTime`, `utc_iso8601`), which allocates and is
//   console-facing.
//
// Re-exports below keep the ~190 `timer::uptime_us` call sites unchanged.

use alloc::string::String;
use alloc::format;

#[cfg(not(kernel_profile_extreme))]
use akuma_primitives::irq;
#[cfg(not(kernel_profile_extreme))]
use akuma_timer::ArchHw;

pub use akuma_timer::read_frequency;
pub use akuma_timer::uptime_us;

pub fn init() {
    // Nothing left to do at init now: the PL031 is read on demand by
    // `init_utc_from_rtc`, and the timer hardware is armed by
    // `enable_timer_interrupts`.
}

// Enable timer interrupts for preemptive scheduling.
// interval_us: interval in microseconds between interrupts
pub fn enable_timer_interrupts(interval_us: u64) {
    akuma_timer::set_tick_us(interval_us);
    akuma_timer::arm_periodic_tick();
}

/// The scheduler tick the host probe chose (or the compiled default / debug
/// override). The ISR and secondary cores read this every re-arm, so a
/// governor demotion takes effect on the next tick without a broadcast.
pub fn current_tick_us() -> u64 {
    akuma_timer::tick_us(crate::config::TIMER_INTERVAL_US)
}

/// BSP idle/netpoll loop iterations — the runtime governor's spin sensor.
/// Healthy: ~1 iteration per timer tick (the loop halts in WFI between ticks).
/// Host that stopped honouring WFI: hundreds of thousands per window
/// (measured ~1.8M/s at a 1 ms tick on the regression host). Incremented from
/// the async-main loop (`main.rs`), read/swapped by the governor block in the
/// ISR below.
pub static NETPOLL_ITERS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Governor observation cadence: every this-many ticks, feed the idle-loop
/// iteration count to `akuma_timer::policy::governor_observe` and demote the
/// tick if the host has stopped honouring WFI. 2000 ticks ≈ 2–6 s depending
/// on the chosen tick.
const GOVERNOR_WINDOW_TICKS: u64 = 2000;
static GOVERNOR_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Timer interrupt handler - called from IRQ handler
pub fn timer_irq_handler(_irq: u32) {
    // Acknowledge interrupt by setting next compare value. The next deadline
    // is computed from the ENTRY counter (not post-work), so a handler that
    // overruns its own interval shortens the next period rather than
    // silently collapsing the tick.
    let freq = akuma_timer::read_frequency();
    let interval_ticks = akuma_timer::ticks_from_us(freq, current_tick_us());
    let counter = akuma_timer::read_counter();
    let new_cval = counter + interval_ticks;

    akuma_cpu::vtimer::set_cval(new_cval);
    // Defensively re-enable the timer on every tick: bit 0 = enable,
    // bit 1 = !mask. If cntv_ctl_el0 ever gets corrupted (enable cleared
    // or mask set), no further IRQs would fire, causing a permanent
    // freeze. Writing 1 here ensures the timer keeps ticking even if
    // something corrupted the control register.
    akuma_cpu::vtimer::set_ctl(1);

    // This periodic virtual-timer tick is the single hardware timer for the
    // kernel. Besides driving preemption (the scheduler SGI below), it services
    // the async alarm queue (SSH read timeouts, Timer::after) which no longer
    // owns the timer hardware itself — see akuma_exec::alarms::update_hardware_timer.
    akuma_exec::alarms::on_timer_interrupt();

    // Check preemption watchdog - detect threads that hold preemption disabled too long
    if crate::config::ENABLE_PREEMPTION_WATCHDOG
        && let Some(duration_us) = akuma_exec::threading::check_preemption_watchdog() {
            // Log warning
            // Use AtomicU64 instead of static mut to avoid data races
            static LAST_WARN_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            let now = uptime_us();
            let last = LAST_WARN_US.load(core::sync::atomic::Ordering::Relaxed);
            // Rate-limit warnings to once per second
            if now.saturating_sub(last) > 1_000_000 {
                LAST_WARN_US.store(now, core::sync::atomic::Ordering::Relaxed);
                // Get poll step to help diagnose where we're stuck
                let step = crate::GLOBAL_POLL_STEP.load(core::sync::atomic::Ordering::Relaxed);
                // Use stack-only print to avoid heap allocation in IRQ context
                let tid = akuma_exec::threading::current_thread_id();
                crate::safe_print!(96, "[WATCHDOG] Preemption disabled for {}ms at step {} tid={}\n",
                    duration_us / 1000, step, tid);
                // Name the call site that took this thread's disable count 0->1 —
                // the culprit holding preemption off (file:line, stack-only print).
                if let Some(loc) = akuma_exec::threading::preemption_disabled_at(tid) {
                    crate::safe_print!(160, "[WATCHDOG] disabled at {}:{}\n", loc.file(), loc.line());
                }
            }
        }

    // NOTE: cleanup_terminated() is NOT called here because it allocates/deallocates
    // memory which could deadlock if main code is in the middle of an allocation.
    // Cleanup should be done from user code via threading::cleanup_terminated().

    if crate::config::TIMER_TICK_HEARTBEAT {
        static TIMER_TICK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let tick = TIMER_TICK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let forking = akuma_exec::process::FORK_IN_PROGRESS.load(core::sync::atomic::Ordering::Relaxed);
        let interval = if forking { 100 } else { 1000 };
        if tick.is_multiple_of(interval) {
            let tid = akuma_exec::threading::current_thread_id();
            let pdis = akuma_exec::threading::preemption_disabled_count(tid);
            crate::safe_print!(96, "[TMR] t={} T={} p={} f={}\n", tick, tid, pdis, u8::from(forking));
        }
    }

    // Trigger SGI for scheduling - scheduler will decide if switch is needed.
    // Real shared-kernel SMP: this timer handler is shared across cores (one dispatch
    // table), so it must ring the CURRENT core's scheduler SGI, not the hardcoded PE0
    // that `trigger_sgi` targets — otherwise a secondary's tick would preempt the BSP.
    // On the BSP (aff0 = 0) `trigger_sgi_self` is equivalent to `trigger_sgi`.
    //
    // Runtime governor (before the SGI so a demotion lands on this very tick's
    // re-arm path next interrupt): every GOVERNOR_WINDOW_TICKS, check whether
    // the idle loop has been spinning — the host may stop honouring WFI after
    // boot (load, heuristic shift), which silently converts idle loops into
    // busy-polls. Demotion is latched and one-way.
    {
        let n = GOVERNOR_TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        if n.is_multiple_of(GOVERNOR_WINDOW_TICKS) {
            let idle_iters = NETPOLL_ITERS.swap(0, core::sync::atomic::Ordering::Relaxed);
            if let Some(new_us) =
                akuma_timer::policy::governor_observe(idle_iters, GOVERNOR_WINDOW_TICKS)
            {
                akuma_timer::set_tick_us(new_us);
                crate::safe_print!(96, "[Timer] governor: WFI spin detected, tick -> {} us\n", new_us);
            }
        }
    }
    #[cfg(kernel_smp_shared)]
    crate::gic::trigger_sgi_self(crate::gic::SGI_SCHEDULER);
    #[cfg(not(kernel_smp_shared))]
    crate::gic::trigger_sgi(crate::gic::SGI_SCHEDULER);
}

// ============================================================================
// Boot-time host-WFI probe
// ============================================================================

/// The core currently running [`probe_host_tick`], or `u32::MAX` when no probe
/// is in flight. Read by [`probe_irq_nop`] to tell its own one-shot apart from
/// a *secondary's* periodic tick landing in the same shared handler slot.
static PROBING_CORE: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(u32::MAX);

/// NOP IRQ handler installed while the probe measures WFI: the exception
/// dispatcher acks/EOIs around us, but the handler itself must disarm the
/// timer — a one-shot that has fired keeps its level asserted (CVAL <=
/// counter, enabled), and an unmasked level re-forwards forever after EOI.
/// The next probe sample re-arms.
///
/// **Only the probing core may disarm.** IRQ 27 is a per-CPU PPI, but
/// `irq::register_handler` writes ONE shared dispatch table (`src/irq.rs:42`),
/// so every *secondary's* periodic tick lands here too for as long as the probe
/// runs. That is not the hypothetical this function's contract below assumes:
/// `bringup_secondaries()` (main.rs:849) has had the secondaries online,
/// armed and IRQ-unmasked for 100+ lines by the time the probe starts
/// (main.rs:955). Disarming there stops that core's timer *permanently* — a
/// secondary arms its tick exactly once (smp_shared.rs:952), and the only thing
/// that would re-arm it is `timer_irq_handler`'s defensive write, which needs a
/// tick it can now never take. Every secondary then sits in WFI forever: online,
/// never preempted, never entering the scheduler, so all work stays on core 0
/// (`smp_shared_{scheduler,userspace,migration}` FAILED, `core1=0`; regression
/// bisected to 38345eb7, which introduced the probe).
///
/// A secondary therefore re-arms its periodic tick and returns. It loses only
/// the scheduler SGI of whatever ticks fall inside the probe's ~50 ms window.
pub fn probe_irq_nop(_irq: u32) {
    if akuma_exec::bkl::current_core_id()
        != PROBING_CORE.load(core::sync::atomic::Ordering::Relaxed)
    {
        akuma_timer::arm_periodic_tick();
        return;
    }
    akuma_timer::disarm();
}

/// Probe the host's WFI behaviour and return the scheduler tick to use.
///
/// Requires: IRQ 27 registered (as [`probe_irq_nop`]) and enabled at the GIC.
/// Runs with IRQs briefly unmasked (saved/restored): each sample needs the
/// one-shot timer IRQ actually delivered, or WFI would never wake. Only the
/// BSP calls this; the chosen tick is published via the crate's `set_tick_us`
/// and every core re-arms from it.
///
/// It does **not** run before secondaries exist — `bringup_secondaries()` is
/// main.rs:849, this is main.rs:955 — so it publishes the probing core for
/// [`probe_irq_nop`], which shares its handler slot with every secondary's
/// live periodic tick. See that function for what happens without it.
///
/// Skipped (returns the compiled default) on `kernel_profile_extreme`: a 4 MB
/// single-core box keeps its historical 10 ms and needs no probing.
pub fn probe_host_tick() -> u64 {
    #[cfg(kernel_profile_extreme)]
    {
        crate::config::TIMER_INTERVAL_US
    }
    #[cfg(not(kernel_profile_extreme))]
    {
        // Publish before unmasking: the first sample's IRQ can land immediately.
        PROBING_CORE.store(
            akuma_exec::bkl::current_core_id(),
            core::sync::atomic::Ordering::Relaxed,
        );
        let saved_daif = irq::irq_save_mask();
        irq::unmask_irqs_sync();
        let picked = akuma_timer::policy::pick_tick(&ArchHw);
        irq::irq_restore(saved_daif);
        PROBING_CORE.store(u32::MAX, core::sync::atomic::Ordering::Relaxed);
        crate::safe_print!(96, "[Timer] host WFI probe: tick = {} us\n", picked);
        picked
    }
}

// ============================================================================
// UTC / presentation
// ============================================================================

// Read Unix timestamp from PL031 RTC (seconds since Unix epoch)
// Returns None if RTC is not initialized
pub fn read_rtc_timestamp() -> Option<u32> {
    #[cfg(target_os = "none")]
    {
        akuma_timer::rtc::unix_seconds()
    }
    #[cfg(not(target_os = "none"))]
    {
        None
    }
}

// Initialize UTC time from PL031 RTC
// Returns true if successful, false if RTC not available
pub fn init_utc_from_rtc() -> bool {
    if let Some(timestamp) = read_rtc_timestamp() {
        // Convert seconds to microseconds
        let unix_epoch_us = u64::from(timestamp) * 1_000_000;
        akuma_timer::set_utc_time_us(unix_epoch_us, uptime_us());
        true
    } else {
        false
    }
}

// Get current UTC time in microseconds since Unix epoch
// Returns None if UTC time has not been set
pub fn utc_time_us() -> Option<u64> {
    akuma_timer::utc_time_us(uptime_us())
}

// Get current UTC time in seconds since Unix epoch
// Returns None if UTC time has not been set
// Used by TLS certificate verification
pub fn utc_seconds() -> Option<u64> {
    utc_time_us().map(|us| us / 1_000_000)
}

// DateTime structure for ISO 8601 formatting
#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
}

impl DateTime {
    // Convert microseconds since Unix epoch to DateTime
    pub fn from_unix_us(us: u64) -> Self {
        let secs = us / 1_000_000;
        let micros = (us % 1_000_000) as u32;

        // Days since Unix epoch
        let mut days = secs / 86400;
        let secs_today = secs % 86400;

        // Time of day
        let hour = (secs_today / 3600) as u8;
        let minute = ((secs_today % 3600) / 60) as u8;
        let second = (secs_today % 60) as u8;

        // Calculate year (starting from 1970)
        let mut year = 1970;
        loop {
            let year_days = if is_leap_year(year) { 366 } else { 365 };
            if days < year_days {
                break;
            }
            days -= year_days;
            year += 1;
        }

        let months = if is_leap_year(year) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };

        let mut month = 1;
        for &month_days in &months {
            if days < month_days as u64 {
                break;
            }
            days -= month_days as u64;
            month += 1;
        }

        let day = (days + 1) as u8;

        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            microsecond: micros,
        }
    }

    pub fn to_iso8601(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.microsecond
        )
    }
}

// Check if a year is a leap year
fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

// Get current UTC time as ISO 8601
// Returns "NOT_SET" if UTC time hasn't been configured
pub fn utc_iso8601() -> String {
    match utc_time_us() {
        Some(us) => DateTime::from_unix_us(us).to_iso8601(),
        None => String::from("NOT_SET"),
    }
}
