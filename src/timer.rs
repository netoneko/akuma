// Timer ISR + presentation shim.
//
// The hardware half (CNTV access, PL031 RTC, UTC offset) and the self-tuning
// tick policy live in the `akuma-timer` crate (extracted 2026-08-18 per
// docs/archive/TRIM_FAT_EMBARRESSING_DUPLICATIONS.md's deferred-audit row:
// this file used to be scheduler-ISR logic wearing a driver's filename). What
// remains here is fused to the bin crate and cannot move:
//
// - `timer_irq_handler` — the ISR: re-arms the periodic tick, services the
//   `kernel_timer` alarm queue, runs the preemption watchdog, feeds the
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

// AB-PROBE: tick-cost instrumentation. Remove before landing.
pub static PROBE_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PROBE_LAST_ENTRY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PROBE_BODY_SUM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PROBE_PERIOD_SUM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// BSP idle/netpoll loop iterations — the runtime governor's spin sensor.
/// Healthy: ~1 iteration per timer tick (the loop halts in WFI between ticks).
/// Host that stopped honouring WFI: hundreds of thousands per window
/// (measured ~1.8M/s at a 1 ms tick on the regression host). Incremented from
/// the async-main loop (`main.rs`), read/swapped by the TICKPROBE block below.
pub static NETPOLL_ITERS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Timer interrupt handler - called from IRQ handler
pub fn timer_irq_handler(_irq: u32) {
    let probe_entry = akuma_timer::read_counter();
    // Acknowledge interrupt by setting next compare value. The next deadline
    // is computed from the ENTRY counter (not post-work), so a handler that
    // overruns its own interval shows up as a shortened period in TICKPROBE
    // instead of silently collapsing the tick.
    let freq = akuma_timer::read_frequency();
    let interval_ticks = akuma_timer::ticks_from_us(freq, current_tick_us());
    let counter = akuma_timer::read_counter();
    let new_cval = counter + interval_ticks;

    unsafe {
        core::arch::asm!("msr cntv_cval_el0, {}", in(reg) new_cval);
        // Defensively re-enable the timer on every tick: bit 0 = enable,
        // bit 1 = !mask. If cntv_ctl_el0 ever gets corrupted (enable cleared
        // or mask set), no further IRQs would fire, causing a permanent
        // freeze. Writing 1 here ensures the timer keeps ticking even if
        // something corrupted the control register.
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64);
    }

    // This periodic virtual-timer tick is the single hardware timer for the
    // kernel. Besides driving preemption (the scheduler SGI below), it services
    // the async alarm queue (SSH read timeouts, Timer::after) which no longer
    // owns the timer hardware itself — see kernel_timer::update_hardware_timer.
    crate::kernel_timer::on_timer_interrupt();

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
    // AB-PROBE
    {
        let end = akuma_timer::read_counter();
        let last = PROBE_LAST_ENTRY.swap(probe_entry, core::sync::atomic::Ordering::Relaxed);
        PROBE_BODY_SUM.fetch_add(end.saturating_sub(probe_entry), core::sync::atomic::Ordering::Relaxed);
        if last > 0 {
            PROBE_PERIOD_SUM.fetch_add(probe_entry.saturating_sub(last), core::sync::atomic::Ordering::Relaxed);
        }
        let n = PROBE_TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        if n.is_multiple_of(2000) {
            let f = freq / 1_000_000; // ticks per us
            let body = PROBE_BODY_SUM.swap(0, core::sync::atomic::Ordering::Relaxed) / 2000 / f.max(1);
            let period = PROBE_PERIOD_SUM.swap(0, core::sync::atomic::Ordering::Relaxed) / 2000 / f.max(1);
            let netiter = NETPOLL_ITERS.swap(0, core::sync::atomic::Ordering::Relaxed);
            crate::safe_print!(128, "[TICKPROBE] n={} tick_us={} body_us={} period_us={} idle_iters={}\n",
                n, current_tick_us(), body, period, netiter);
            // Runtime governor: if the idle loop is spinning (host stopped
            // honouring WFI since boot), demote the tick. Takes effect on the
            // very next re-arm above.
            if let Some(new_us) = akuma_timer::policy::governor_observe(netiter, 2000) {
                akuma_timer::set_tick_us(new_us);
                crate::safe_print!(96, "[TICKPROBE] governor: WFI spin detected, tick -> {} us\n", new_us);
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

/// NOP IRQ handler installed while the probe measures WFI: the exception
/// dispatcher acks/EOIs around us, but the handler itself must disarm the
/// timer — a one-shot that has fired keeps its level asserted (CVAL <=
/// counter, enabled), and an unmasked level re-forwards forever after EOI.
/// The next probe sample re-arms.
pub fn probe_irq_nop(_irq: u32) {
    akuma_timer::disarm();
}

/// Probe the host's WFI behaviour and return the scheduler tick to use.
///
/// Requires: IRQ 27 registered (as [`probe_irq_nop`]) and enabled at the GIC.
/// Runs with IRQs briefly unmasked (saved/restored): each sample needs the
/// one-shot timer IRQ actually delivered, or WFI would never wake. Only the
/// BSP calls this, before secondaries exist; the chosen tick is published via
/// the crate's `set_tick_us` and every core re-arms from it.
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
        let saved_daif = irq::irq_save_mask();
        irq::unmask_irqs_sync();
        let picked = akuma_timer::policy::pick_tick(&ArchHw);
        irq::irq_restore(saved_daif);
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
