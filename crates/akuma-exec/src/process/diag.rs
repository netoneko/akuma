//! Process table diagnostics: lock timing and borrow tracking.
//!
//! All features are compile-time gated via `const bool` flags.
//! When disabled, LLVM eliminates all diagnostic code paths entirely.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::runtime::runtime;

// ============================================================================
// Lock-hold-time tracking
// ============================================================================

/// Enable lock-hold-time tracking on PROCESS_TABLE.
pub const LOCK_TIMING_ENABLED: bool = true;

/// Log if PROCESS_TABLE is held longer than this (microseconds).
pub const LOCK_HOLD_THRESHOLD_US: u64 = 100;

/// Measure and log the duration of a PROCESS_TABLE lock hold.
/// Call this around existing lock sites:
///   `let t = lock_timer_start(); ... lock_timer_end("caller", t);`
#[inline]
pub fn lock_timer_start() -> u64 {
    if LOCK_TIMING_ENABLED { (runtime().uptime_us)() } else { 0 }
}

/// Finish timing a lock hold. Logs `[PTLOCK]` if over threshold.
#[inline]
pub fn lock_timer_end(caller: &str, t0: u64) {
    if LOCK_TIMING_ENABLED {
        let elapsed = (runtime().uptime_us)().saturating_sub(t0);
        if elapsed > LOCK_HOLD_THRESHOLD_US {
            log_slow_lock(caller, elapsed);
        }
    }
}

fn log_slow_lock(caller: &str, elapsed_us: u64) {
    use core::fmt::Write;
    let mut buf = StackBuf::<128>::new();
    let _ = writeln!(buf, "[PTLOCK] {}: held {}us", caller, elapsed_us);
    buf.flush();
}

// ============================================================================
// Borrow-aliasing detector
// ============================================================================

/// Enable the borrow-aliasing detector for lookup_process().
pub const BORROW_TRACKING_ENABLED: bool = true;

/// Per-PID outstanding borrow count. Index = PID value.
const MAX_TRACKED_PIDS: usize = 256;
static BORROW_COUNTS: [AtomicU32; MAX_TRACKED_PIDS] = {
    const INIT: AtomicU32 = AtomicU32::new(0);
    [INIT; MAX_TRACKED_PIDS]
};

/// Increment borrow count for a PID. Logs if aliasing detected (count >= 2).
#[inline]
pub fn borrow_inc(pid: u32) {
    if !BORROW_TRACKING_ENABLED { return; }
    let idx = pid as usize;
    if idx >= MAX_TRACKED_PIDS { return; }
    let prev = BORROW_COUNTS[idx].fetch_add(1, Ordering::Relaxed);
    if prev >= 1 {
        log_borrow_alias(pid, prev + 1);
    }
}

/// Decrement borrow count for a PID.
#[inline]
pub fn borrow_dec(pid: u32) {
    if !BORROW_TRACKING_ENABLED { return; }
    let idx = pid as usize;
    if idx >= MAX_TRACKED_PIDS { return; }
    BORROW_COUNTS[idx].fetch_sub(1, Ordering::Relaxed);
}

/// RAII guard that decrements borrow count on drop.
pub struct BorrowGuard {
    pid: u32,
}

impl Drop for BorrowGuard {
    fn drop(&mut self) {
        borrow_dec(self.pid);
    }
}

/// Look up a process with explicit RAII borrow tracking.
/// New code should prefer this over `lookup_process()`.
pub fn lookup_process_tracked(pid: crate::process::types::Pid) -> Option<(&'static mut crate::process::Process, BorrowGuard)> {
    let proc = crate::process::children::lookup_process(pid)?;
    let guard = BorrowGuard { pid };
    // borrow_inc is already called inside lookup_process, so we skip it here
    // and just provide the guard for decrement
    Some((proc, guard))
}

fn log_borrow_alias(pid: u32, count: u32) {
    use core::fmt::Write;
    let mut buf = StackBuf::<128>::new();
    let _ = writeln!(buf, "[BORROW-ALIAS] pid={} count={}", pid, count);
    buf.flush();
}

// ============================================================================
// Stack-based print buffer (no heap allocation)
// ============================================================================

struct StackBuf<const N: usize> {
    buf: [u8; N],
    pos: usize,
}

impl<const N: usize> StackBuf<N> {
    fn new() -> Self {
        Self { buf: [0u8; N], pos: 0 }
    }

    fn flush(&self) {
        if self.pos > 0 {
            if let Ok(s) = core::str::from_utf8(&self.buf[..self.pos]) {
                (runtime().print_str)(s);
            }
        }
    }
}

impl<const N: usize> core::fmt::Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = N - self.pos;
        let to_copy = bytes.len().min(avail);
        self.buf[self.pos..self.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.pos += to_copy;
        Ok(())
    }
}
