//! Per-pid syscall trace rings — the data behind `/proc/<pid>/syscalls`.
//!
//! # Why this is a crate
//!
//! Not for host tests, and not to localise `unsafe` (it has none). It moved out
//! of `src/syscall/` on 2026-09-01 to **break a dependency cycle**:
//! `src/vfs/proc.rs` read `crate::syscall::log`, and `src/syscall/` reads 37
//! symbols back out of `src/vfs/`. Cargo crates cannot be mutually dependent, so
//! that loop had to be cut before either directory could leave the binary
//! (`docs/archive/SRC_SYSCALL_EXTRACTION.md`, Blocker 1).
//!
//! It sits below both: the syscall epilogue writes, `/proc` reads, and neither
//! needs to know about the other.
//!
//! # The retained-log hazard this type exists to contain
//!
//! **A log outlives the process that wrote it** — that is the entire point, so
//! `/proc/<pid>/syscalls` still answers after the process is gone. Two
//! consequences drive the shape of the code below, and both were real bugs:
//!
//! 1. The process table cannot be asked who owned a retained log, so the owning
//!    box is recorded in the entry. Without it the isolation check had nothing
//!    to consult once the process exited and **fell open** — a container read
//!    `/proc/1/syscalls`.
//! 2. A pid can be recycled while the previous occupant's log is still retained.
//!    [`record`] therefore clears on a box change rather than appending, or a
//!    container handed a recycled pid inherits a host process's trace.
//!
//! Visibility is decided in [`get_formatted`], never by callers: the file is
//! reachable through four `ProcFilesystem` methods, and when each carried its
//! own copy of the rule only one had it — `read_file`, the one a `read()` never
//! reaches (`docs/archive/DEVBOX_ISSUES.md` Issue 24).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spinning_top::Spinlock;

use akuma_primitives::clock::uptime_us;
use akuma_primitives::irq::with_irqs_disabled;

/// Tunables, handed over once at boot.
///
/// `src/config.rs` stays the single source of truth — `src/syscall/log.rs` is a
/// shim that reads the consts and calls [`init`]. Do **not** add a second copy
/// here. The cost of the indirection is that two const-folded comparisons became
/// relaxed atomic loads; both sit behind the `epi.log` epilogue gate and a
/// spinlock acquisition, so it is noise.
#[derive(Clone, Copy, Debug)]
pub struct LogConfig {
    /// Ring depth per pid. Older entries are dropped first.
    pub max_entries: usize,
    /// How long a log survives its process, in milliseconds.
    pub retain_ms: u64,
}

static MAX_ENTRIES: AtomicUsize = AtomicUsize::new(64);
static RETAIN_MS: AtomicU64 = AtomicU64::new(10_000);

/// Install the tunables. Idempotent; safe to skip entirely in host tests, where
/// the defaults above match `src/config.rs`.
pub fn init(cfg: LogConfig) {
    MAX_ENTRIES.store(cfg.max_entries, Ordering::Relaxed);
    RETAIN_MS.store(cfg.retain_ms, Ordering::Relaxed);
}

fn max_entries() -> usize {
    MAX_ENTRIES.load(Ordering::Relaxed)
}

fn retain_us() -> u64 {
    RETAIN_MS.load(Ordering::Relaxed) * 1_000
}

struct SyscallEntry {
    timestamp_us: u64,
    nr: u64,
    duration_us: u64,
    result: u64,
}

struct ProcessSyscallLog {
    entries: VecDeque<SyscallEntry>,
    exited_at_us: Option<u64>,
    /// The box that produced these entries.
    ///
    /// The log outlives the process — that is its whole point — so the process
    /// table cannot be asked who owned a retained log. Without this recorded
    /// here, the isolation check had nothing to consult once the process was
    /// gone and fell open, which is how a container came to read `/proc/1/syscalls`.
    box_id: u64,
}

static SYSCALL_LOG: Spinlock<BTreeMap<u32, ProcessSyscallLog>> =
    Spinlock::new(BTreeMap::new());

pub fn record(pid: u32, box_id: u64, nr: u64, timestamp_us: u64, duration_us: u64, result: u64) {
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        let entry = log.entry(pid).or_insert_with(|| ProcessSyscallLog {
            entries: VecDeque::new(),
            exited_at_us: None,
            box_id,
        });
        // A different box writing under the same pid means the pid was recycled
        // while the previous occupant's log was still retained. Start over rather
        // than append: otherwise the new process inherits the old one's trace, and
        // a container handed a recycled pid would inherit a host process's.
        if entry.box_id != box_id {
            entry.entries.clear();
            entry.exited_at_us = None;
            entry.box_id = box_id;
        }
        if entry.entries.len() >= max_entries() {
            entry.entries.pop_front();
        }
        entry.entries.push_back(SyscallEntry { timestamp_us, nr, duration_us, result });
    });
}

pub fn mark_exited(pid: u32) {
    let now = uptime_us();
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        if let Some(entry) = log.get_mut(&pid) {
            entry.exited_at_us = Some(now);
        }
    });
}

/// Render `pid`'s retained log, or `None` if there is none, it has expired, or
/// `viewer_box_id` is not allowed to see it.
///
/// **Visibility is decided here, not by the callers.** `/proc/<pid>/syscalls` is
/// reachable through four `ProcFilesystem` methods, and when each carried its own
/// copy of the rule only one of them actually had it — and it was `read_file`,
/// the one a `read()` never reaches (docs/archive/DEVBOX_ISSUES.md Issue 24).
#[must_use]
pub fn get_formatted(pid: u32, viewer_box_id: u64) -> Option<Vec<u8>> {
    let now = uptime_us();
    
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();

        // Lazily remove expired entries
        log.retain(|_, v| {
            if let Some(exited_at) = v.exited_at_us {
                now.saturating_sub(exited_at) < retain_us()
            } else {
                true
            }
        });

        let entry = log.get(&pid)?;

        // Box 0 is the host and sees every log; a box sees only its own.
        if viewer_box_id != 0 && entry.box_id != viewer_box_id {
            return None;
        }

        // Check if expired
        if let Some(exited_at) = entry.exited_at_us
            && now.saturating_sub(exited_at) >= retain_us() {
                return None;
            }

        let mut out = format!("# pid={pid}\n# TIMESTAMP_US       NR  DUR_US  RESULT\n");
        for e in &entry.entries {
            let _ = writeln!(out,
                "  {:19}  {:3}  {:6}  {:6}",
                e.timestamp_us, e.nr, e.duration_us, e.result
            );
        }
        Some(out.into_bytes())
    })
}

/// Every pid with a live retained log that `viewer_box_id` may see.
#[must_use]
pub fn list_pids_with_logs(viewer_box_id: u64) -> Vec<u32> {
    let now = uptime_us();
    
    with_irqs_disabled(|| {
        let log = SYSCALL_LOG.lock();
        log.iter()
            .filter(|(_, v)| {
                if viewer_box_id != 0 && v.box_id != viewer_box_id {
                    return false;
                }
                if let Some(exited_at) = v.exited_at_us {
                    now.saturating_sub(exited_at) < retain_us()
                } else {
                    true
                }
            })
            .map(|(pid, _)| *pid)
            .collect()
    })
}
