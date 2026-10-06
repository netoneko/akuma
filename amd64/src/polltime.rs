//! Time for polled drivers: the TSC rate, millisecond budgets, and the one
//! busy-wait every controller poll goes through.
//!
//! Moved out of `xhci.rs` (2026-10-06) when the NVMe driver became the second
//! polled controller. The wait is not just a delay: it is where a core stuck
//! polling a device still answers TLB shootdowns and lets the boot splash draw
//! (`spin_us`), so a second driver with its own copy would have been a second
//! place to forget that.

/// The TSC rate every budget below is written against.
///
/// `lapic::calibrate` measures it across the same 10 ms PIT gate as the LAPIC
/// count, so a polled driver's "one second" and the scheduler's agree. When no PIT
/// was there to measure against (Firecracker, `microvm`) the fallback is the
/// **fastest** part this kernel could plausibly meet, so a budget is never
/// shorter than asked — the failure that matters here is a slow drive read as
/// dead, not a dead drive read as slow.
///
/// History: the budgets used to be a bare `1_000_000_000` ticks, assuming a
/// TSC of at least 1 GHz. On the trashcan's 3.2 GHz Haswell that "second" was
/// 0.31 s. A drive waking from standby answers in seconds, so the first
/// command after every idle gap "timed out", the recovery ran against a device
/// that was merely busy, and much of the stall cadence on that box was this
/// constant (`docs/archive/AKUMA_AMD64_USB_XHCI.md` § "Clock finding").
const TSC_HZ_FALLBACK: u64 = 4_000_000_000;

pub fn tsc_hz() -> u64 {
    match crate::lapic::tsc_hz() {
        0 => TSC_HZ_FALLBACK,
        hz => hz,
    }
}

/// TSC ticks in `ms` milliseconds.
pub fn ticks_ms(ms: u64) -> u64 {
    tsc_hz() / 1000 * ms
}

/// Busy-wait `us` microseconds.
///
/// Every poll in the xHCI and NVMe drivers — command completion, each
/// transfer phase, the recovery ladder — waits through here, so this is where the TLB-shootdown
/// assist goes. The driver runs IRQ-masked (syscall context) and, on the exec
/// and file-read paths, **with the BKL dropped**, so a stalled phase used to
/// be up to 10 s in which this core could acknowledge nobody's shootdown; the
/// BKL holder waiting on it froze every other core for as long as the disk
/// did (`crate::shootdown::masked_wait_assist`).
pub fn spin_us(us: u64) {
    let target = tsc_hz() / 1_000_000 * us;
    // SAFETY: RDTSC is unprivileged and present on all x86_64.
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start) < target {
        crate::shootdown::masked_wait_assist();
        // The USB bring-up is the longest synchronous stretch of a boot before the
        // scheduler runs: let a quiet boot's splash draw a frame from here (a no-op
        // unless one is due and up).
        crate::splash::pulse();
        core::hint::spin_loop();
    }
}

pub fn tsc() -> u64 {
    // SAFETY: as `spin_us`.
    unsafe { core::arch::x86_64::_rdtsc() }
}
